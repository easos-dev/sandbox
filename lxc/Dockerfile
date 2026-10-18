# LXC 容器模板：在已构建的沙箱镜像之上加 systemd 与网络工具，导出根文件系统就是 Proxmox / LXC 的容器模板。
# 只在打模板时用（scripts/build-lxc-template.sh），不是运行镜像；BASE 指向 Dockerfile 构建出的沙箱镜像。
ARG BASE=easit2/easos-sandbox:latest
FROM ${BASE}
ENV DEBIAN_FRONTEND=noninteractive
# systemd-sysv 提供 /sbin/init（LXC 的 PID 1）；ifupdown 与 DHCP 客户端让 Proxmox 写进 /etc/network/interfaces 的网络配置生效。
# Debian 13 里 isc-dhcp-client 可能已被 dhcpcd-base 取代，两个都试；静态地址不需要 DHCP 客户端。
RUN apt-get update \
 && apt-get install -y --no-install-recommends systemd systemd-sysv ifupdown iproute2 \
 && (apt-get install -y --no-install-recommends isc-dhcp-client || apt-get install -y --no-install-recommends dhcpcd-base) \
 && rm -rf /var/lib/apt/lists/* \
 # 模板里不能带固定的 machine-id：留空文件，每个容器首次启动时自己生成。
 && truncate -s 0 /etc/machine-id \
 && rm -f /var/lib/dbus/machine-id
COPY lxc/easos-sandbox.service /etc/systemd/system/easos-sandbox.service
COPY lxc/env /etc/easos-sandbox/env
RUN chmod 644 /etc/systemd/system/easos-sandbox.service /etc/easos-sandbox/env \
 && systemctl enable easos-sandbox.service
# 模板的 PID 1 是 systemd（它负责回收孤儿进程，不再需要 tini）；运行镜像的 ENTRYPOINT 在这里不再有意义。
ENTRYPOINT ["/sbin/init"]
