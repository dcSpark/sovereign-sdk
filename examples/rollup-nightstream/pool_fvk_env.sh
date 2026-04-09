#!/usr/bin/env bash

# Sourceable helper to make POOL_FVK_PK behavior consistent across scripts.
#
# Resolution rules:
# 1) If POOL_FVK_PK is explicitly set (even to an empty string), respect it.
#    - Empty string disables enforcement.
# 2) Otherwise, if MIDNIGHT_FVK_SERVICE_SIGNING_PK_HEX is set, export it as POOL_FVK_PK.
# 3) Otherwise, leave POOL_FVK_PK unset (enforcement disabled).

normalize_bind_host() {
  local host="${1:-}"
  if [[ "$host" == "0.0.0.0" || "$host" == "::" ]]; then
    echo "127.0.0.1"
    return
  fi
  echo "$host"
}

midnight_fvk_service_url() {
  local explicit_url="${MIDNIGHT_FVK_SERVICE_URL:-}"
  if [[ -n "$explicit_url" ]]; then
    printf "%s" "${explicit_url%/}"
    return 0
  fi

  local bind="${MIDNIGHT_FVK_SERVICE_BIND:-127.0.0.1:8088}"
  local host="${bind%:*}"
  local port="${bind##*:}"
  if [[ "$host" == "$port" ]]; then
    host="$bind"
    port="8088"
  fi
  host="$(normalize_bind_host "$host")"
  printf "http://%s:%s" "$host" "$port"
}

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

wait_for_midnight_fvk_service() {
  local pool_pk="${POOL_FVK_PK:-}"
  if [[ -z "$pool_pk" ]]; then
    return 0
  fi

  if ! command -v curl >/dev/null 2>&1; then
    echo "[config] WARNING: curl not found; skipping midnight-fvk-service readiness wait"
    return 0
  fi

  local timeout_secs="${MIDNIGHT_FVK_SERVICE_READY_TIMEOUT_SECS:-30}"
  local interval_secs="${MIDNIGHT_FVK_SERVICE_READY_INTERVAL_SECS:-1}"
  local base_url
  base_url="$(midnight_fvk_service_url)"
  local health_url="${base_url%/}/health"
  local deadline=$((SECONDS + timeout_secs))

  echo "[config] waiting for midnight-fvk-service at $health_url"
  while (( SECONDS < deadline )); do
    if curl --silent --fail "$health_url" >/dev/null 2>&1; then
      echo "[config] midnight-fvk-service is ready"
      return 0
    fi
    sleep "$interval_secs"
  done

  echo "[config] ERROR: midnight-fvk-service did not become ready at $health_url within ${timeout_secs}s"
  return 1
}
