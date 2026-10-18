#!/bin/bash
# 容器入口：核对密钥配置、用 iptables 配好命令账号的出站规则、永久收回 NET_ADMIN，再以 root 运行运行器。
#
# 规则按发包进程的 uid 区分：
#   root（uid 0，运行器自己）不受限——它只连助手交来的下载/上传地址，其中可能有内网的对象存储。
#   其他 uid（命名空间账号，模型的命令）走 SANDBOX_CMD 链：
#     1. 禁止访问本机上运行器的端口（密钥不在命令环境里，这是第二道锁）；其余回环流量放行，
#        模型自己在沙箱里起的本地服务照常可用。所以拒绝清单里不放 127.0.0.0/8。
#     2. 拒绝清单非空时，放行 /etc/resolv.conf 里的 DNS（Docker 内嵌 DNS 127.0.0.11 本就在回环；
#        宿主 DNS 可能落在私网段，不放行就解析不了域名）。
#     3. 拒绝 SANDBOX_DENY_NETWORKS 里的网段。
#     4. 其余（公网）放行。
# 配完用 capsh 把 cap_net_admin 从 bounding set 里去掉：之后容器内任何进程（包括 root）都改不了规则。
# 没有 NET_ADMIN（iptables 不可用）时只打警告照常启动：规则是部署的事，运行器本身不依赖它。
set -euo pipefail

# 默认拒绝清单：与助手读网页的内网拒绝清单一致，去掉回环（见上面第 1 条）。
#   10/8、172.16/12、192.168/16  私网：数据库、ES、对象存储与 Docker 分配的容器网段都在这里
#                               （Docker 默认只从 172.17～172.31 与 192.168 里分网段），所以不再单独加容器自身网段
#   100.64/10                   运营商 NAT 与部分云厂商内网
#   169.254/16                  链路本地，含云主机元数据服务
#   33/8                        助手内网拒绝清单里的内部网段
#   0/8、224/4、240/4           本网、组播与保留段
#   fc00::/7、fe80::/10          IPv6 私网与链路本地
DEFAULT_DENY_NETWORKS="10.0.0.0/8,172.16.0.0/12,192.168.0.0/16,100.64.0.0/10,169.254.0.0/16,33.0.0.0/8,0.0.0.0/8,224.0.0.0/4,240.0.0.0/4,fc00::/7,fe80::/10"

if [ -z "${SANDBOX_API_KEY:-}" ] && [ -z "${SANDBOX_API_KEY_FILE:-}" ]; then
    echo "需要 SANDBOX_API_KEY 或 SANDBOX_API_KEY_FILE" >&2
    exit 1
fi

# 密钥文件必须只有 root 读得到。Docker Desktop（macOS / Windows）的文件共享不执行宿主文件的权限位：
# 挂进来的 600 文件在容器里显示为 root:root 600，但任何 uid 都能读（2026-10-18 实测）。所以密钥文件要挂在
# 只有 root 能进的目录下（例如 /root/.sandbox-key），靠容器自己文件系统上 /root 的 700 挡住命令账号。
# 这里以 nobody 的身份真读一次，读得到就拒绝启动，免得部署时挂错位置把密钥暴露给模型的命令。
if [ -n "${SANDBOX_API_KEY_FILE:-}" ]; then
    if setpriv --reuid=65534 --regid=65534 --clear-groups cat "$SANDBOX_API_KEY_FILE" >/dev/null 2>&1; then
        echo "密钥文件 $SANDBOX_API_KEY_FILE 普通账号也读得到，拒绝启动：请挂在只有 root 能进的目录下（例如 /root/.sandbox-key）" >&2
        exit 1
    fi
fi

# 监听端口：取 SANDBOX_LISTEN 冒号后面的部分（默认 8000）。
LISTEN="${SANDBOX_LISTEN:-0.0.0.0:8000}"
PORT="${LISTEN##*:}"

# 拒绝清单：未设置时用默认值；设成空串表示不拒绝任何网段（仍禁止访问运行器端口）。
# 逗号或空白分隔；按有没有冒号分成 IPv4 与 IPv6 两组，格式不对就拒绝启动，不带着残缺的规则运行。
DENY_NETWORKS="${SANDBOX_DENY_NETWORKS-$DEFAULT_DENY_NETWORKS}"
DENY4=()
DENY6=()
for cidr in ${DENY_NETWORKS//,/ }; do
    if [[ "$cidr" =~ ^[0-9]{1,3}(\.[0-9]{1,3}){3}(/[0-9]{1,2})?$ ]]; then
        DENY4+=("$cidr")
    elif [[ "$cidr" == *:* && "$cidr" =~ ^[0-9A-Fa-f:]+(/[0-9]{1,3})?$ ]]; then
        DENY6+=("$cidr")
    else
        echo "SANDBOX_DENY_NETWORKS 里的 \"$cidr\" 不是 CIDR 网段，拒绝启动" >&2
        exit 1
    fi
done

# 向一个表（iptables 或 ip6tables）写入 SANDBOX_CMD 链；$2 之后是要拒绝的网段。
setup_chain() {
    local ipt="$1"; shift
    local reject_with="$1"; shift
    "$ipt" -N SANDBOX_CMD 2>/dev/null || "$ipt" -F SANDBOX_CMD
    "$ipt" -A SANDBOX_CMD -o lo -p tcp --dport "$PORT" -j REJECT --reject-with tcp-reset
    "$ipt" -A SANDBOX_CMD -o lo -j RETURN
    if [ $# -gt 0 ]; then
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
            "$ipt" -A SANDBOX_CMD -d "$cidr" -j REJECT --reject-with "$reject_with" || {
                echo "网段 $cidr 写不进 $ipt（取值无效？），拒绝启动" >&2
                exit 1
            }
        done
    fi
    "$ipt" -A SANDBOX_CMD -j RETURN
    "$ipt" -A OUTPUT -m owner ! --uid-owner 0 -j SANDBOX_CMD
}

if command -v iptables >/dev/null 2>&1 && iptables -L OUTPUT -n >/dev/null 2>&1; then
    setup_chain iptables icmp-port-unreachable "${DENY4[@]}"
    if command -v ip6tables >/dev/null 2>&1 && ip6tables -L OUTPUT -n >/dev/null 2>&1; then
        setup_chain ip6tables icmp6-port-unreachable "${DENY6[@]}"
    elif [ ${#DENY6[@]} -gt 0 ]; then
        echo "警告：ip6tables 不可用（容器可能没有 IPv6），IPv6 网段的拒绝规则未写入" >&2
    fi
    if [ ${#DENY4[@]} -eq 0 ] && [ ${#DENY6[@]} -eq 0 ]; then
        echo "出站规则已配置：拒绝清单为空，命令账号可访问任何网段，仍禁止访问运行器端口 $PORT"
    else
        echo "出站规则已配置：命令账号拒绝 ${DENY4[*]} ${DENY6[*]}，禁止访问运行器端口 $PORT"
    fi
    echo "收回 NET_ADMIN 后启动运行器"
    exec capsh --drop=cap_net_admin -- -c 'exec /usr/local/bin/sandbox-runner'
fi

echo "警告：iptables 不可用（容器没有 NET_ADMIN？），不配置出站规则，命令账号的网络不受限" >&2
exec /usr/local/bin/sandbox-runner
