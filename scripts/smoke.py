#!/usr/bin/env python3
"""镜像冒烟：起容器，走一遍
健康检查、python3 数据栈、以命名空间账号执行与干净环境、写文件后 stat、跨命名空间隔离、
命令账号的公网可达 / 默认拒绝清单（私网、33 段）/ 运行器端口拒绝 / 读不到密钥文件、运行器已收回 NET_ADMIN、
LibreOffice 转 PDF、超时终止、镜像工具逐项核对（scripts/tools-check.sh，以命名空间账号执行）、
容器重建后账号沿用旧家目录的 UID 且新账号不抢号、删会话，
以及另起容器检查：拒绝清单设为空、拒绝清单写错、密钥文件挂错位置。
打印镜像大小与每步耗时，最后删掉容器、临时卷与临时密钥文件。

密钥与正式部署一样用文件（SANDBOX_API_KEY_FILE），挂在容器里的 /root 下。

用法：python3 scripts/smoke.py [镜像名] [-- 额外的 docker run 参数…]（镜像默认 easos-sandbox:dev）。
需要本机 docker 与宿主机 18000 端口空闲。`--` 之后的参数原样插在 `docker run` 的镜像名之前（例如 --network）。
"""

import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request

ARGS = sys.argv[1:]
EXTRA_RUN_ARGS = ARGS[ARGS.index("--") + 1 :] if "--" in ARGS else []
POSITIONAL = ARGS[: ARGS.index("--")] if "--" in ARGS else ARGS
IMAGE = POSITIONAL[0] if POSITIONAL else "easos-sandbox:dev"
NAME = f"easos-sandbox-smoke-{os.getpid()}"
# 具名临时卷：容器重建那一项要靠它保留家目录；结束时删掉（匿名卷不会随 docker rm -f 删除，会越积越多）。
VOLUME = f"easos-sandbox-smoke-{os.getpid()}"
KEY = f"smoke-{int(time.time())}"
# 冒烟用的密钥文件放在仓库忽略的 .checks/ 里，跑完删掉；容器里挂在只有 root 能进的 /root 下。
KEY_FILE = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), ".checks", f"smoke-key-{os.getpid()}")
KEY_IN_CONTAINER = "/root/.sandbox-key"
# 工具核对脚本：整段作为命令交给 /execute，以命名空间账号在容器里跑。
TOOLS_CHECK = os.path.join(os.path.dirname(os.path.abspath(__file__)), "tools-check.sh")
# 宿主机上映射的端口；容器内固定 8000。
PORT = 18000
BASE = f"http://127.0.0.1:{PORT}"
# 命名空间与会话：与真实调用同形。
NS = "user-smoke"
NEWCOMER = "user-newcomer"
CONV = "smoke1"
FAILED: list[str] = []


def sh(*args: str) -> str:
    return subprocess.run(args, check=True, capture_output=True, text=True).stdout


def docker_run(name: str, *extra: str, publish: bool = True, key_path: str = KEY_IN_CONTAINER) -> None:
    """起一个沙箱容器；extra 是额外的 docker run 参数（例如 -e）。"""
    subprocess.run(["docker", "rm", "-f", "-v", name], capture_output=True)
    ports = ["-p", f"127.0.0.1:{PORT}:8000"] if publish else []
    sh(
        "docker", "run", "-d", "--name", name, "--cap-add", "NET_ADMIN", *ports,
        "-v", f"{KEY_FILE}:{key_path}:ro", "-e", f"SANDBOX_API_KEY_FILE={key_path}",
        *extra, *EXTRA_RUN_ARGS, IMAGE,
    )


def wait_healthy() -> None:
    for _ in range(100):
        try:
            urllib.request.urlopen(BASE + "/healthz", timeout=1)
            return
        except Exception:
            time.sleep(0.2)
    raise SystemExit("运行器没有起来：\n" + sh("docker", "logs", NAME))


def wait_exit(name: str) -> tuple[str, str]:
    """等一个预期会自己退出的容器结束，返回（退出码, 日志）。"""
    code = sh("docker", "wait", name).strip()
    logs = subprocess.run(["docker", "logs", name], capture_output=True, text=True)
    return code, (logs.stdout + logs.stderr).strip()


def call(method: str, path: str, body=None, ns: str = NS):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(
        BASE + path,
        data=data,
        method=method,
        headers={"Authorization": f"Bearer {KEY}", "X-User-Id": ns, "Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(req, timeout=120) as response:
            return response.status, json.loads(response.read())
    except urllib.error.HTTPError as error:
        return error.code, json.loads(error.read())


def run(command: str, timeout: int | None = None, ns: str = NS):
    """像助手客户端那样执行：开始后按 offset 轮询到结束，返回（终态, 拼起来的输出）。"""
    body = {"conversation_id": CONV, "command": command}
    if timeout:
        body["timeout"] = timeout
    status, view = call("POST", "/execute?wait=30", body, ns=ns)
    assert status == 200, view
    output = "".join(item["data"] for item in view["output"])
    while view["status"] == "running":
        status, view = call("GET", f"/execute/{view['id']}/status?wait=30&offset={view['next_offset']}", ns=ns)
        assert status == 200, view
        output += "".join(item["data"] for item in view["output"])
    return view, output


def check(name: str, ok: bool, detail: str = ""):
    print(f"{'通过' if ok else '失败'}  {name}  {detail}".rstrip())
    if not ok:
        FAILED.append(name)


def refused_quickly(target: str) -> tuple[bool, str]:
    """命令账号连 target：被 REJECT 的连接在毫秒内失败（curl 退出码 7）；没有规则时会等到超时（退出码 28）。"""
    started = time.time()
    _, out = run(f"curl -sS -o /dev/null -m 5 {target} 2>&1; echo exit=$?")
    elapsed = time.time() - started
    return "exit=7" in out and elapsed < 2, f"{out.strip().replace(chr(10), ' | ')}  {elapsed:.1f}s"


def side_checks() -> None:
    """另起容器检查拒绝清单的两种配置与密钥文件挂错位置；它们不对外映射端口，也不挂卷。"""
    name = NAME + "-side"
    try:
        docker_run(name, "-e", "SANDBOX_DENY_NETWORKS=", publish=False)
        for _ in range(50):
            if "启动运行器" in subprocess.run(["docker", "logs", name], capture_output=True, text=True).stdout:
                break
            time.sleep(0.2)
        rules = sh("docker", "exec", name, "iptables", "-S", "SANDBOX_CMD")
        rejects = [line for line in rules.splitlines() if "REJECT" in line]
        check("拒绝清单设为空时只剩运行器端口一条拒绝", len(rejects) == 1 and "--dport 8000" in rejects[0], " | ".join(rejects))

        docker_run(name, "-e", "SANDBOX_DENY_NETWORKS=10.0.0.0/8,not-a-cidr", publish=False)
        code, text = wait_exit(name)
        check("拒绝清单写错时拒绝启动", code != "0" and "not-a-cidr" in text, f"退出码 {code}：{text}")

        docker_run(name, publish=False, key_path="/tmp/sandbox-key")
        code, text = wait_exit(name)
        check("密钥文件挂在普通账号读得到的位置时拒绝启动", code != "0" and "拒绝启动" in text, f"退出码 {code}：{text}")
    finally:
        subprocess.run(["docker", "rm", "-f", "-v", name], capture_output=True)


def main():
    os.makedirs(os.path.dirname(KEY_FILE), exist_ok=True)
    fd = os.open(KEY_FILE, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as handle:
        handle.write(KEY)
    sh("docker", "volume", "create", VOLUME)
    try:
        docker_run(NAME, "-v", f"{VOLUME}:/home")
        wait_healthy()
        print(sh("docker", "logs", NAME).strip())
        size = int(sh("docker", "image", "inspect", IMAGE, "--format", "{{.Size}}").strip())
        print(f"镜像 {IMAGE} 大小 {size / 1e6:.0f} MB")

        started = time.time()
        view, out = run("python3 -c 'import pandas, numpy, openpyxl, matplotlib, docx, pptx, pypdf; print(pandas.__version__, numpy.__version__)'")
        check("python3 数据栈可用", view["status"] == "done" and view["exit_code"] == 0, f"{out.strip()}  {time.time() - started:.1f}s")

        view, out = run("id -un; pwd; echo $HOME; env | grep -c SANDBOX_ || true; id -u")
        lines = out.strip().split("\n")
        old_uid = lines[4] if len(lines) > 4 else ""
        check(
            "以命名空间账号执行、工作目录与环境正确",
            view["exit_code"] == 0 and lines[:4] == [NS, f"/home/{NS}/conversations/{CONV}", f"/home/{NS}", "0"],
            " | ".join(lines),
        )

        view, out = run("printf 'hello world' > out/a.txt && stat -c '%U %a' out/a.txt && stat -c %a /home/" + NS)
        check("命令写的文件归命名空间账号、家目录 0700", view["exit_code"] == 0 and out.split("\n")[:2] == [f"{NS} 644", "700"], out.strip().replace("\n", " | "))
        status, stat = call("POST", "/files/stat", {"conversation_id": CONV, "path": "out/a.txt"})
        check("stat 指纹", status == 200 and stat.get("md5") == "5eb63bbbe01eeed093cb22bb8f5acdc3" and stat.get("size") == 11 and stat.get("mime") == "text/plain", json.dumps(stat, ensure_ascii=False))

        status, other = call("POST", "/files/stat", {"conversation_id": CONV, "path": "out/a.txt"}, ns="user-other")
        check("别的命名空间看不到这个会话", status == 404 and other.get("error") == "conversation_not_found", json.dumps(other, ensure_ascii=False))
        view, peek = run(f"ls /home/{NS}/ >/dev/null 2>&1 && echo readable || echo denied", ns="user-other")
        check("别的账号读不到这个家目录", peek.strip() == "denied", peek.strip())

        started = time.time()
        view, out = run("curl -sS -o /dev/null -w '%{http_code}' -m 20 https://example.com/ || echo curl-failed")
        check("命令账号可访问公网", out.strip() in ("200", "301", "302"), f"{out.strip()}  {time.time() - started:.1f}s")
        ok, detail = refused_quickly("http://10.255.255.1/")
        check("命令账号访问私网被拒", ok, detail)
        ok, detail = refused_quickly("http://33.0.0.1/")
        check("命令账号访问 33 段被拒", ok, detail)
        view, out = run("curl -sS -m 5 http://127.0.0.1:8000/healthz 2>&1; echo exit=$?")
        check("命令账号访问运行器端口被拒", '"ok"' not in out and "exit=7" in out, out.strip().replace("\n", " | "))
        view, out = run(f"cat {KEY_IN_CONTAINER} >/dev/null 2>&1 && echo readable || echo denied; env | grep -c KEY || true")
        check("命令账号读不到密钥文件、环境里没有密钥变量", out.split() == ["denied", "0"], out.strip().replace("\n", " | "))
        rules = sh("docker", "exec", NAME, "iptables", "-S", "OUTPUT")
        check("出站规则只对非 root 生效", "! --uid-owner 0 -j SANDBOX_CMD" in rules, rules.strip().replace("\n", " | "))
        chain = sh("docker", "exec", NAME, "iptables", "-S", "SANDBOX_CMD")
        denied = [line.split()[3] for line in chain.splitlines() if "REJECT" in line and " -d " in line]
        expected = ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "100.64.0.0/10", "169.254.0.0/16", "33.0.0.0/8", "0.0.0.0/8", "224.0.0.0/4", "240.0.0.0/4"]
        check("默认拒绝清单完整", denied == expected, " ".join(denied))
        pid = sh("docker", "exec", NAME, "pgrep", "-x", "sandbox-runner").strip().split()[0]
        bnd = sh("docker", "exec", NAME, "grep", "CapBnd", f"/proc/{pid}/status").split()[1]
        check("运行器已收回 NET_ADMIN（CapBnd 第 12 位为 0）", int(bnd, 16) & (1 << 12) == 0, f"CapBnd={bnd}")

        started = time.time()
        view, out = run("printf '中文測試 日本語テスト\\n' > out/t.txt && soffice --headless --convert-to pdf --outdir out out/t.txt >/dev/null 2>&1; ls out/t.pdf")
        check("LibreOffice 转 PDF", view["exit_code"] == 0 and "out/t.pdf" in out, f"{out.strip()}  {time.time() - started:.1f}s")
        status, stat = call("POST", "/files/stat", {"conversation_id": CONV, "path": "out/t.pdf"})
        check("PDF 的 MIME 推断", stat.get("mime") == "application/pdf", json.dumps(stat.get("mime")))

        view, out = run("sleep 20", timeout=1)
        check("超时终止", view["status"] == "timeout", view["status"])

        started = time.time()
        with open(TOOLS_CHECK, encoding="utf-8") as script:
            view, out = run(script.read())
        for line in out.splitlines():
            if line.startswith(("通过  ", "失败  ")):
                verdict, _, rest = line.partition("  ")
                item, _, detail = rest.partition("  ")
                check(f"工具：{item}", verdict == "通过", detail)
        check("工具核对脚本跑完", view["status"] == "done", f"{view['status']}，退出码 {view.get('exit_code')}，{time.time() - started:.0f}s")

        # 容器重建：账号表随容器层消失，家目录留在卷里。先让一个新账号来，它不能拿走旧账号的 UID；
        # 再让旧账号来，它要沿用旧家目录的 UID，看得到自己的旧文件，新账号看不到它的家目录。
        docker_run(NAME, "-v", f"{VOLUME}:/home")
        wait_healthy()
        view, new_uid = run("id -u", ns=NEWCOMER)
        view, out = run("id -u; cat out/a.txt")
        lines = out.strip().split("\n")
        check(
            "容器重建后旧账号沿用原 UID、读得到旧文件，新账号不抢号",
            old_uid != "" and lines == [old_uid, "hello world"] and new_uid.strip() != old_uid,
            f"旧 UID {old_uid}，重建后 {' | '.join(lines)}，新账号 UID {new_uid.strip()}",
        )
        view, peek = run(f"ls /home/{NS}/ >/dev/null 2>&1 && echo readable || echo denied", ns=NEWCOMER)
        check("容器重建后新账号读不到旧账号的家目录", peek.strip() == "denied", peek.strip())

        status, deleted = call("DELETE", f"/conversations/{CONV}")
        check("删会话", status == 200 and deleted.get("deleted") is True, json.dumps(deleted))
        status, gone = call("POST", "/files/stat", {"conversation_id": CONV, "path": "out/a.txt"})
        check("删后会话不存在", status == 404, json.dumps(gone, ensure_ascii=False))

        side_checks()
    finally:
        subprocess.run(["docker", "rm", "-f", "-v", NAME], capture_output=True)
        subprocess.run(["docker", "volume", "rm", "-f", VOLUME], capture_output=True)
        if os.path.exists(KEY_FILE):
            os.remove(KEY_FILE)
    if FAILED:
        raise SystemExit(f"冒烟失败：{FAILED}")
    print("冒烟全部通过")


if __name__ == "__main__":
    main()
