#!/bin/bash
# 容器入口：核对密钥配置、用 iptables 配好命令账号的出站规则、永久收回 NET_ADMIN，再以 root 运行运行器。
#
# 规则按发包进程的 uid 区分：
#   root（uid 0，运行器自己）不受限——它只连助手交来的下载/上传地址，其中可能有内网的对象存储。
#   其他 uid（命名空间账号，模型的命令）走 SANDBOX_CMD 链：
#     1. 禁止访问本机上运行器的端口（密钥不在命令环境里，这是第二道锁）；其余回环流量放行。
#     2. 放行 /etc/resolv.conf 里的 DNS（Docker 内嵌 DNS 127.0.0.11 本就在回环；宿主 DNS 可能落在私网段）。
#     3. 除非 SANDBOX_ALLOW_PRIVATE=1：拒绝私网、链路本地、运营商 NAT 段与容器自身所在的网段。
#     4. 其余（公网）放行。
# 配完用 capsh 把 cap_net_admin 从 bounding set 里去掉：之后容器内任何进程（包括 root）都改不了规则。
# 没有 NET_ADMIN（iptables 不可用）时只打警告照常启动：规则是部署的事，运行器本身不依赖它。
set -euo pipefail

if [ -z "${SANDBOX_API_KEY:-}" ] && [ -z "${SANDBOX_API_KEY_FILE:-}" ]; then
    echo "需要 SANDBOX_API_KEY 或 SANDBOX_API_KEY_FILE" >&2
    exit 1
fi

# 监听端口：取 SANDBOX_LISTEN 冒号后面的部分（默认 8000）。
LISTEN="${SANDBOX_LISTEN:-0.0.0.0:8000}"
PORT="${LISTEN##*:}"
ALLOW_PRIVATE="${SANDBOX_ALLOW_PRIVATE:-0}"

# 向一个表（iptables 或 ip6tables）写入 SANDBOX_CMD 链；$2 之后是要拒绝的网段。
setup_chain() {
    local ipt="$1"; shift
    local reject_with="$1"; shift
    "$ipt" -N SANDBOX_CMD 2>/dev/null || "$ipt" -F SANDBOX_CMD
    "$ipt" -A SANDBOX_CMD -o lo -p tcp --dport "$PORT" -j REJECT --reject-with tcp-reset
    "$ipt" -A SANDBOX_CMD -o lo -j RETURN
    if [ "$ALLOW_PRIVATE" != "1" ]; then
        local ns
        for ns in $(awk '/^nameserver/ {print $2}' /etc/resolv.conf); do
            # IPv4 地址写进 iptables、IPv6 地址写进 ip6tables，写错表会报错。
            case "$ipt:$ns" in
                iptables:*:*) continue ;;
                ip6tables:*.*) continue ;;
            esac
            "$ipt" -A SANDBOX_CMD -d "$ns" -p udp --dport 53 -j RETURN
            "$ipt" -A SANDBOX_CMD -d "$ns" -p tcp --dport 53 -j RETURN
        done
        local cidr
        for cidr in "$@"; do
            "$ipt" -A SANDBOX_CMD -d "$cidr" -j REJECT --reject-with "$reject_with"
        done
    fi
    "$ipt" -A SANDBOX_CMD -j RETURN
    "$ipt" -A OUTPUT -m owner ! --uid-owner 0 -j SANDBOX_CMD
}

if command -v iptables >/dev/null 2>&1 && iptables -L OUTPUT -n >/dev/null 2>&1; then
    # 容器自己所在的网段（每块非回环网卡的 IPv4 地址/掩码，iptables 会按掩码归一成网段）。
    CONTAINER_NETS=$(ip -o -4 addr show scope global 2>/dev/null | awk '{print $4}' || true)
    # shellcheck disable=SC2086
    setup_chain iptables icmp-port-unreachable \
        10.0.0.0/8 172.16.0.0/12 192.168.0.0/16 169.254.0.0/16 100.64.0.0/10 $CONTAINER_NETS
    if command -v ip6tables >/dev/null 2>&1 && ip6tables -L OUTPUT -n >/dev/null 2>&1; then
        CONTAINER_NETS6=$(ip -o -6 addr show scope global 2>/dev/null | awk '{print $4}' || true)
        # shellcheck disable=SC2086
        setup_chain ip6tables icmp6-port-unreachable fc00::/7 fe80::/10 $CONTAINER_NETS6 || \
            echo "警告：ip6tables 规则未能全部写入（容器可能没有 IPv6）" >&2
    fi
    if [ "$ALLOW_PRIVATE" = "1" ]; then
        echo "出站规则已配置：命令账号可访问私网（SANDBOX_ALLOW_PRIVATE=1），仍禁止访问运行器端口 $PORT"
    else
        echo "出站规则已配置：命令账号拒绝私网、链路本地与容器网段，禁止访问运行器端口 $PORT"
    fi
    echo "收回 NET_ADMIN 后启动运行器"
    exec capsh --drop=cap_net_admin -- -c 'exec /usr/local/bin/sandbox-runner'
fi

echo "警告：iptables 不可用（容器没有 NET_ADMIN？），不配置出站规则，命令账号的网络不受限" >&2
exec /usr/local/bin/sandbox-runner
