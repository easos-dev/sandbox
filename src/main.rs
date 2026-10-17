//! 沙箱运行器：替代 Open Terminal 的守护进程。给助手后端一套只传 JSON 的 HTTP 接口：以命名空间账号执行命令、
//! 把网上文件下载进会话目录、算文件指纹与持有证明、按上传计划把文件 PUT 出去。文件字节只在浏览器、对象存储与
//! 本沙箱之间流动，运行器从不把文件正文交给调用方。
//! 模块分工：config 读环境变量；server 做 hyper 服务、路由与统一错误；auth 核对密钥；account 管命名空间账号；
//! paths 管会话 ID、相对路径与不跟随符号链接的逐级打开；exec 管命令执行；files 管文件接口；checksum 与 mime 是纯计算。
//! 多用户模式下以 root 运行（要 useradd 与切换账号），网络隔离由 entrypoint.sh 用 iptables 按 uid 配置。

mod account;
mod auth;
mod checksum;
mod config;
mod exec;
mod files;
mod mime;
mod paths;
mod server;

use anyhow::Context;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Arc::new(config::Config::from_env()?);
    let state = Arc::new(server::State::new(cfg.clone()).context("初始化失败")?);
    let listener = tokio::net::TcpListener::bind(cfg.listen)
        .await
        .with_context(|| format!("监听 {} 失败", cfg.listen))?;
    let local = listener.local_addr()?;
    println!(
        "沙箱运行器监听 {local}，{}，家目录根 {}，状态目录 {}",
        if cfg.single_user { "单用户模式" } else { "多用户模式" },
        cfg.home_root.display(),
        cfg.state_dir.display()
    );
    let mut shutdown = std::pin::pin!(shutdown_signal());
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = match accepted {
                    Ok(accepted) => accepted,
                    Err(e) => {
                        eprintln!("接受连接失败：{e}");
                        continue;
                    }
                };
                let state = state.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |req| server::handle(state.clone(), req));
                    if let Err(e) = http1::Builder::new()
                        .keep_alive(true)
                        .serve_connection(TokioIo::new(stream), service)
                        .await
                    {
                        // 客户端半路断开很常见，不值得记；其余连接层错误记一行。
                        if !e.is_incomplete_message() {
                            eprintln!("连接处理出错：{e}");
                        }
                    }
                });
            }
            _ = &mut shutdown => break,
        }
    }
    state.executions.shutdown().await;
    println!("沙箱运行器退出");
    Ok(())
}

/// 等 SIGTERM（docker stop）或 Ctrl-C。
async fn shutdown_signal() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("注册 SIGTERM 处理");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    println!("收到退出信号");
}
