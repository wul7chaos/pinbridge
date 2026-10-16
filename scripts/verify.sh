#!/usr/bin/env bash
# PinBridge 本地全量验证（与 CI 逐条对应）。
# 用法：  bash scripts/verify.sh
#         SKIP_PYTHON=1 bash scripts/verify.sh
#
# 与 scripts/verify.ps1 等价；两个脚本的存在是为了"Windows 上别逼别人装 bash、
# Linux/macOS 上别逼别人装 pwsh"。它们是同一份清单的两个入口，
# 修改门禁时**必须同时改两个**（CI 用的是第三条路径，见 .github/workflows/ci.yml）。

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

pass=0
step() {
  local name="$1"; shift
  local fix="$1"; shift
  echo
  echo "== ${name} =="
  local start=$SECONDS
  if "$@"; then
    echo "ok ($((SECONDS - start))s)"
    pass=$((pass + 1))
  else
    echo "FAIL —— ${fix}" >&2
    exit 1
  fi
}

step "cargo fmt --check" "运行 cargo fmt --all" cargo fmt --all -- --check
step "cargo clippy -D warnings" "按提示修代码，不要改 lint 配置" \
  cargo clippy --workspace --all-targets -- -D warnings
step "cargo test --workspace" "先看第一个失败的用例" cargo test --workspace
step "端到端自检（pinbridge demo）" "检查 demo 断在哪一步" \
  cargo run --quiet -p pinbridge-cli -- demo --trace-dir "${TMPDIR:-/tmp}/pinbridge-verify"

if [[ "${SKIP_PYTHON:-0}" != "1" ]] && command -v python3 >/dev/null 2>&1; then
  step "Python 侧（pytest）" "跨语言一致性失败时先定位是哪一侧偏离规范" \
    env PYTHONPATH=python/src python3 -m pytest python/tests -q
else
  echo "· 跳过 Python 侧（CI 会跑）"
fi

echo
echo "全部通过（${pass} 步）。别忘了更新 CHANGELOG.md。"
