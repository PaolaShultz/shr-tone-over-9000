#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
account_home="$(getent passwd "$(id -u)" | cut -d: -f6)"
install_prefix="${PREFIX:-${account_home}/.local}"
models_dir="${RPI_TONE_MODELS_DIR:-${XDG_DATA_HOME:-${account_home}/.local/share}/shr-tone-over-9000/models}"
accept_t3k=false
install_models=true
system_deps=false
tone3000_client_id=""
hub_config="${RPI_TONE_HUB_CONFIG:-${XDG_CONFIG_HOME:-${account_home}/.config}/shr-tone-over-9000/hub.conf}"

usage() {
    printf '%s\n' \
        "Usage: scripts/install.sh [OPTIONS]" \
        "" \
        "  --accept-t3k       install the curated TONE3000 NAM files for local use" \
        "  --no-models        install only the application" \
        "  --system-deps      install Debian build/runtime packages with sudo" \
        "  --tone3000-client-id ID  configure this installation's t3k_pub_… ID" \
        "  --prefix DIR       binary prefix (default: ${install_prefix})" \
        "  --models-dir DIR   model/IR library (default: ${models_dir})" \
        "  -h, --help         show this help"
}

while (($#)); do
    case "$1" in
        --accept-t3k) accept_t3k=true ;;
        --no-models) install_models=false ;;
        --system-deps) system_deps=true ;;
        --tone3000-client-id)
            [[ $# -ge 2 ]] || { printf '%s\n' '--tone3000-client-id requires an ID' >&2; exit 2; }
            tone3000_client_id="$2"
            shift
            ;;
        --prefix)
            [[ $# -ge 2 ]] || { printf '%s\n' '--prefix requires a directory' >&2; exit 2; }
            install_prefix="$2"
            shift
            ;;
        --models-dir)
            [[ $# -ge 2 ]] || { printf '%s\n' '--models-dir requires a directory' >&2; exit 2; }
            models_dir="$2"
            shift
            ;;
        -h|--help) usage; exit 0 ;;
        *) printf 'unknown option: %s\n' "$1" >&2; usage >&2; exit 2 ;;
    esac
    shift
done

if [[ -n "$tone3000_client_id" && ! "$tone3000_client_id" =~ ^t3k_pub_[^[:space:]]+$ ]]; then
    printf '%s\n' '--tone3000-client-id must have the form t3k_pub_…' >&2
    exit 2
fi

cd "$repo_dir"

if "$system_deps"; then
    sudo apt-get update
    sudo apt-get install --no-install-recommends \
        build-essential curl git pkg-config libjack-jackd2-dev libasound2-dev \
        nlohmann-json3-dev libeigen3-dev
fi

command -v cargo >/dev/null || {
    printf '%s\n' 'cargo is missing; install rustup and the pinned Rust toolchain first' >&2
    exit 1
}
command -v git >/dev/null || { printf '%s\n' 'git is required' >&2; exit 1; }
command -v curl >/dev/null || { printf '%s\n' 'curl is required' >&2; exit 1; }

core_dir="${repo_dir}/vendor/tone3000-plugin/plugin/NeuralAmpModelerCore"
if [[ ! -d "$core_dir" ]]; then
    mkdir -p "${repo_dir}/vendor"
    plugin_dir="${repo_dir}/vendor/tone3000-plugin"
    if [[ ! -d "${plugin_dir}/.git" ]]; then
        git clone --no-recurse-submodules \
            https://github.com/tone-3000/tone3000-plugin.git "$plugin_dir"
    fi
    git -C "$plugin_dir" submodule update --init plugin/NeuralAmpModelerCore
fi

cargo build --manifest-path "${repo_dir}/Cargo.toml" --release --locked
install -Dm755 "${repo_dir}/target/release/shr-tone-over-9000" \
    "${install_prefix}/bin/shr-tone-over-9000"

if [[ -n "$tone3000_client_id" ]]; then
    install -d -m700 "$(dirname "$hub_config")"
    printf 'client_id=%s\n' "$tone3000_client_id" | install -m600 /dev/stdin "$hub_config"
    printf 'TONE3000 client config: %s\n' "$hub_config"
fi

if "$install_models"; then
    if "$accept_t3k"; then
        "${install_prefix}/bin/shr-tone-over-9000" models install all \
            --models-dir "$models_dir" --accept-t3k
    else
        "${install_prefix}/bin/shr-tone-over-9000" models install cab \
            --models-dir "$models_dir"
        printf '%s\n' \
            'Installed the five CC0 cabinet IRs.' \
            'For the complete curated NAM library, review the source licenses and rerun:' \
            "  scripts/install.sh --accept-t3k --models-dir ${models_dir}"
    fi
fi

printf 'Installed %s\n' "${install_prefix}/bin/shr-tone-over-9000"
if "$install_models"; then
    printf 'Model library: %s\n' "$models_dir"
fi
