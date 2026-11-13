# ===========================================================================
# wist-harvest — 运行镜像（多架构）
#
# 与 `wist-gateway` **同一套做法**：二进制由 release 流水线按平台**原生**编译，
# build context（本目录）按 `amd64/` `arm64/` 提供，Dockerfile 只负责打包成运行镜像。
#
# ⚠ **整个 Dockerfile 不许出现 `RUN`**：它会在目标架构下执行，`linux/arm64` 那一侧
#   只能靠 QEMU 模拟（一次 `apt-get` 就是几分钟，而且没有缓存，每发一个 tag 重付一次）。
#   需要执行的东西要么在 CI 上做完（二进制就是），要么挑自带依赖的基础镜像。
#
# 为什么是 distroless **cc**/debian13（与 gateway 同因）：
#   · 二进制是 **glibc 动态链接**（`-unknown-linux-gnu`），debian12 的 2.36 不够
#     （构建 runner 是 ubuntu-24.04，glibc 2.39），alpine 是 musl 更不行；
#   · **必须是 `cc` 而不是 `base`**：Rust 产物动态链接 `libgcc_s.so.1`（panic 展开用），
#     `base` 变体不含它 —— 容器起来就是 `libgcc_s.so.1` 找不到，退出码 127、无限重启；
#   · `cc` 自带 ca-certificates：云 API 走 HTTPS，reqwest 用 rustls 的平台校验器
#     （rustls-platform-verifier）**要系统根证书**，这里正好有；
#   · 代价：镜像里没有 shell —— harvest 的排障是 `wist-harvest diagnose`
#     （栈里的 healthcheck 就是它），不需要进容器敲命令。
# ===========================================================================

FROM gcr.io/distroless/cc-debian13:nonroot

ARG TARGETARCH

LABEL org.opencontainers.image.source="https://github.com/dayu-sec/wist-harvest"
LABEL org.opencontainers.image.description="Cloud account fact collector (cloud assets → data plane)"
LABEL org.opencontainers.image.licenses="Apache-2.0"

# 预编译二进制（build context 按架构提供：amd64/wist-harvest、arm64/wist-harvest）
COPY --chown=10001:10001 ${TARGETARCH}/wist-harvest /usr/local/bin/wist-harvest

# 运行期以固定的 10001 身份跑：与 `wist-field-stack` 的 compose `user: "10001:10001"` 同值
#（evidence / field-api 也是 10001 —— 宿主侧只读挂载的「属组」是唯一共享通道）。
# 不用 gateway 的 999：harvest 现在跑在计算区那一栈，身份跟那一栈对齐。
USER 10001:10001

# **不 EXPOSE、不内置 HEALTHCHECK**：
#   · 采集器没有入站面，只出网到云 API、出站到数据面的 warp-parse；
#   · `diagnose` 必须带 `--config` / `--source`，而这两个路径由栈挂载决定
#     （`/etc/wist-harvest/{harvest.toml,sources.d/source.toml}`）。镜像里猜一个路径，
#     只会得到一个「看着有健康检查、其实永远失败」的容器 —— 所以健康检查写在栈的 compose 里。
ENTRYPOINT ["wist-harvest"]
CMD []
