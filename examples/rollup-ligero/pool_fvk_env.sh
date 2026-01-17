#!/usr/bin/env bash

# Sourceable helper to make POOL_FVK_PK behavior consistent across scripts.
#
# Resolution rules:
# 1) If POOL_FVK_PK is explicitly set (even to an empty string), respect it.
#    - Empty string disables enforcement.
# 2) Otherwise, if MIDNIGHT_FVK_SERVICE_SIGNING_PK_HEX is set, export it as POOL_FVK_PK.
# 3) Otherwise, leave POOL_FVK_PK unset (enforcement disabled).

resolve_pool_fvk_pk() {
  local explicitly_set=0
  if [[ "${POOL_FVK_PK+x}" == "x" ]]; then
    explicitly_set=1
  fi

  local pool_pk="${POOL_FVK_PK:-}"
  local fvk_service_pk="${MIDNIGHT_FVK_SERVICE_SIGNING_PK_HEX:-}"

  if [[ $explicitly_set -eq 1 ]]; then
    if [[ -n "$pool_pk" && -n "$fvk_service_pk" && "$pool_pk" != "$fvk_service_pk" ]]; then
      echo "[config] WARNING: POOL_FVK_PK != MIDNIGHT_FVK_SERVICE_SIGNING_PK_HEX; verifier enforcement uses POOL_FVK_PK"
    fi
    export POOL_FVK_PK_SOURCE="POOL_FVK_PK"
    return 0
  fi

  if [[ -n "$fvk_service_pk" ]]; then
    export POOL_FVK_PK="$fvk_service_pk"
    export POOL_FVK_PK_SOURCE="MIDNIGHT_FVK_SERVICE_SIGNING_PK_HEX"
    return 0
  fi

  export POOL_FVK_PK_SOURCE="unset"
}

print_pool_fvk_pk_status() {
  local pool_pk="${POOL_FVK_PK:-}"
  local src="${POOL_FVK_PK_SOURCE:-unknown}"
  if [[ -n "$pool_pk" ]]; then
    echo "[config] pool viewer-commitment signature enforcement: ENABLED (POOL_FVK_PK=$pool_pk, source=$src)"
  else
    echo "[config] pool viewer-commitment signature enforcement: DISABLED (POOL_FVK_PK unset/empty)"
  fi
}
