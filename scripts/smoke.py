#!/usr/bin/env python3
"""镜像冒烟：起容器，走一遍健康检查、python3 数据栈、以命名空间账号执行与干净环境、写文件后 stat、
命令账号的公网可达 / 私网拒绝 / 运行器端口拒绝 / 读不到密钥文件、运行器已收回 NET_ADMIN、LibreOffice 转 PDF、
超时终止、删会话，以及密钥文件挂在普通账号读得到的位置时容器拒绝启动；打印镜像大小与每步耗时，最后删容器。
密钥与正式部署一样用文件（SANDBOX_API_KEY_FILE），挂在容器里的 /root 下。

用法：python3 scripts/smoke.py [镜像名] [-- 额外的 docker run 参数…]（镜像默认 easos-sandbox:dev）。
需要本机 docker 与宿主机 18000 端口空闲。`--` 之后的参数原样插在 `docker run` 的镜像名之前（例如 --network、-v）。
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
KEY = f"smoke-{int(time.time())}"
# 冒烟用的密钥文件放在仓库忽略的 .checks/ 里，跑完删掉；容器里挂在只有 root 能进的 /root 下。
KEY_FILE = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), ".checks", f"smoke-key-{os.getpid()}")
KEY_IN_CONTAINER = "/root/.sandbox-key"
# 宿主机上映射的端口；容器内固定 8000。
PORT = 18000
BASE = f"http://127.0.0.1:{PORT}"
# 命名空间与会话：与真实调用同形。
NS = "user-smoke"
CONV = "smoke1"
FAILED: list[str] = []


def sh(*args: str) -> str:
    return subprocess.run(args, check=True, capture_output=True, text=True).stdout


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


def misplaced_key_refused() -> tuple[bool, str]:
    """密钥文件挂在 /tmp（普通账号读得到）时，入口脚本应拒绝启动、容器以非零码退出。"""
    name = NAME + "-misplaced"
    subprocess.run(["docker", "rm", "-f", name], capture_output=True)
    try:
        sh(
            "docker", "run", "-d", "--name", name, "--cap-add", "NET_ADMIN",
            "-v", f"{KEY_FILE}:/tmp/sandbox-key:ro", "-e", "SANDBOX_API_KEY_FILE=/tmp/sandbox-key",
            *EXTRA_RUN_ARGS, IMAGE,
        )
        code = sh("docker", "wait", name).strip()
        logs = subprocess.run(["docker", "logs", name], capture_output=True, text=True)
        text = (logs.stdout + logs.stderr).strip()
        return code != "0" and "拒绝启动" in text, f"退出码 {code}：{text}"
    finally:
        subprocess.run(["docker", "rm", "-f", name], capture_output=True)


def main():
    subprocess.run(["docker", "rm", "-f", NAME], capture_output=True)
    os.makedirs(os.path.dirname(KEY_FILE), exist_ok=True)
    fd = os.open(KEY_FILE, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as handle:
        handle.write(KEY)
    sh(
        "docker", "run", "-d", "--name", NAME, "--cap-add", "NET_ADMIN",
        "-p", f"127.0.0.1:{PORT}:8000",
        "-v", f"{KEY_FILE}:{KEY_IN_CONTAINER}:ro", "-e", f"SANDBOX_API_KEY_FILE={KEY_IN_CONTAINER}",
        *EXTRA_RUN_ARGS, IMAGE,
    )
    try:
        for _ in range(100):
            try:
                urllib.request.urlopen(BASE + "/healthz", timeout=1)
                break
            except Exception:
                time.sleep(0.2)
        else:
            raise SystemExit("运行器没有起来：\n" + sh("docker", "logs", NAME))
        print(sh("docker", "logs", NAME).strip())
        size = int(sh("docker", "image", "inspect", IMAGE, "--format", "{{.Size}}").strip())
        print(f"镜像 {IMAGE} 大小 {size / 1e6:.0f} MB")

        started = time.time()
        view, out = run("python3 -c 'import pandas, numpy, openpyxl, matplotlib, docx, pptx, pypdf; print(pandas.__version__, numpy.__version__)'")
        check("python3 数据栈可用", view["status"] == "done" and view["exit_code"] == 0, f"{out.strip()}  {time.time() - started:.1f}s")

        view, out = run("id -un; pwd; echo $HOME; env | grep -c SANDBOX_ || true")
        lines = out.strip().split("\n")
        check(
            "以命名空间账号执行、工作目录与环境正确",
            view["exit_code"] == 0 and lines[0] == NS and lines[1] == f"/home/{NS}/conversations/{CONV}" and lines[2] == f"/home/{NS}" and lines[3] == "0",
            " | ".join(lines),
        )

        view, out = run("printf 'hello world' > out/a.txt && stat -c '%U %a' out/a.txt && stat -c %a /home/" + NS)
        check("命令写的文件归命名空间账号、家目录 0700", view["exit_code"] == 0 and out.split("\n")[0] == f"{NS} 644" and out.split("\n")[1] == "700", out.strip().replace("\n", " | "))
        status, stat = call("POST", "/files/stat", {"conversation_id": CONV, "path": "out/a.txt"})
        check("stat 指纹", status == 200 and stat.get("md5") == "5eb63bbbe01eeed093cb22bb8f5acdc3" and stat.get("size") == 11 and stat.get("mime") == "text/plain", json.dumps(stat, ensure_ascii=False))

        status, other = call("POST", "/files/stat", {"conversation_id": CONV, "path": "out/a.txt"}, ns="user-other")
        check("别的命名空间看不到这个会话", status == 404 and other.get("error") == "conversation_not_found", json.dumps(other, ensure_ascii=False))
        # 以 user-other 的身份试读 user-smoke 的家目录。
        view, peek_out = run(f"ls /home/{NS}/ >/dev/null 2>&1 && echo readable || echo denied", ns="user-other")
        check("别的账号读不到这个家目录", peek_out.strip() == "denied", peek_out.strip())

        started = time.time()
        view, out = run("curl -sS -o /dev/null -w '%{http_code}' -m 20 https://example.com/ || echo curl-failed")
        check("命令账号可访问公网", out.strip() in ("200", "301", "302"), f"{out.strip()}  {time.time() - started:.1f}s")
        # 被 REJECT 的连接在毫秒内失败（curl 退出码 7）；没有规则时会等到 5 秒超时（退出码 28）。
        started = time.time()
        view, out = run("curl -sS -o /dev/null -m 5 http://10.255.255.1/ 2>&1; echo exit=$?")
        elapsed = time.time() - started
        check("命令账号访问私网被拒", "exit=7" in out and elapsed < 2, f"{out.strip().replace(chr(10), ' | ')}  {elapsed:.1f}s")
        view, out = run("curl -sS -m 5 http://127.0.0.1:8000/healthz 2>&1; echo exit=$?")
        check("命令账号访问运行器端口被拒", '"ok"' not in out and "exit=7" in out, out.strip().replace("\n", " | "))
        view, out = run(f"cat {KEY_IN_CONTAINER} >/dev/null 2>&1 && echo readable || echo denied; env | grep -c KEY || true")
        check("命令账号读不到密钥文件、环境里没有密钥变量", out.split() == ["denied", "0"], out.strip().replace("\n", " | "))
        rules = sh("docker", "exec", NAME, "iptables", "-S", "OUTPUT")
        check("出站规则只对非 root 生效", "! --uid-owner 0 -j SANDBOX_CMD" in rules, rules.strip())
        print("SANDBOX_CMD 链：\n" + sh("docker", "exec", NAME, "iptables", "-S", "SANDBOX_CMD").strip())
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
        status, deleted = call("DELETE", f"/conversations/{CONV}")
        check("删会话", status == 200 and deleted.get("deleted") is True, json.dumps(deleted))
        status, gone = call("POST", "/files/stat", {"conversation_id": CONV, "path": "out/a.txt"})
        check("删后会话不存在", status == 404, json.dumps(gone, ensure_ascii=False))
        ok, detail = misplaced_key_refused()
        check("密钥文件挂在普通账号读得到的位置时拒绝启动", ok, detail)
    finally:
        subprocess.run(["docker", "rm", "-f", NAME], capture_output=True)
        if os.path.exists(KEY_FILE):
            os.remove(KEY_FILE)
    if FAILED:
        raise SystemExit(f"冒烟失败：{FAILED}")
    print("冒烟全部通过")


if __name__ == "__main__":
    main()
