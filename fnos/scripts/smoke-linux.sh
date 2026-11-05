#!/bin/bash
# Exercise both production processes against a bind-mounted fixture inside a
# private Linux mount namespace. Never write the real system hosts file.
set -euo pipefail
if [ "${1:-}" != --inside ]; then
    [ "$(id -u)" = 0 ] || { echo 'Run the smoke test as root inside a disposable Linux test environment.' >&2; exit 1; }
    exec unshare --mount --propagation private bash "$0" --inside "${1:?Pass the Linux server binary}"
fi
binary="$(readlink -f "${2:?}")"
lifecycle="$(cd "$(dirname "${BASH_SOURCE[0]}")/../package/cmd" && pwd)/main"
test_root="$(mktemp -d /var/tmp/swh-fnos-smoke.XXXXXX)"
export TRIM_APPDEST="$test_root/target"
export TRIM_PKGVAR="$test_root/var"
export TRIM_USERNAME=nobody
runtime_mounted=false
hosts_mounted=false
cleanup() {
    if [ "$runtime_mounted" = true ]; then
        bash "$lifecycle" stop || true
        umount /run 2>/dev/null || true
    fi
    if [ "$hosts_mounted" = true ]; then umount /etc/hosts 2>/dev/null || true; fi
    # The literal mktemp prefix and canonical path are verified before deletion.
    case "$(readlink -f "$test_root")" in /var/tmp/swh-fnos-smoke.*) rm -rf -- "$test_root" ;; esac
}
trap cleanup EXIT
chmod 0755 "$test_root"
mkdir -p "$test_root/installed/server" "$TRIM_PKGVAR"
ln -s installed "$TRIM_APPDEST"
cp "$binary" "$TRIM_APPDEST/server/switchhosts-fnos"
# fnOS may assign the installed target directory to the package user. Exercise
# the actual lifecycle bootstrap, including securing that directory for app.sock.
chown nobody:nogroup "$TRIM_APPDEST" "$TRIM_PKGVAR"
chmod 0775 "$TRIM_APPDEST"
printf '127.0.0.1 localhost\n192.168.1.9 preserved.test\n' > "$test_root/hosts"
mount --bind "$test_root/hosts" /etc/hosts
hosts_mounted=true
# Hide the host's runtime directory before invoking the fixed /run paths in
# cmd/main, so an existing SwitchHosts installation cannot be affected.
mount -t tmpfs -o mode=0755,nosuid,nodev tmpfs /run
runtime_mounted=true
if ! bash "$lifecycle" start; then
    cat /run/switchhosts-fnos/*.log >&2
    exit 1
fi
bash "$lifecycle" status
[ "$(stat -L -c '%u:%g:%a' "$TRIM_APPDEST")" = 0:0:755 ]
[ "$(stat -c '%u' "$TRIM_PKGVAR/data")" = 65534 ]
[ "$(stat -c '%u:%a' "$TRIM_APPDEST/app.sock")" = 0:600 ]
read -r web_pid _ < /run/switchhosts-fnos/web.pid
read -r writer_pid _ < /run/switchhosts-fnos/writer.pid
socket="$TRIM_APPDEST/app.sock"
url=http://localhost/app/switchhosts/api
status="$(curl --silent --unix-socket "$socket" -o /dev/null -w '%{http_code}' "$url/state")"
[ "$status" = 403 ]
status="$(curl --silent --unix-socket "$socket" -o /dev/null -w '%{http_code}' -H 'X-Trim-Userid: 1000' -H 'X-Trim-Isadmin: false' "$url/state")"
[ "$status" = 403 ]
if runuser -u nobody -- curl --silent --unix-socket "$socket" -H 'X-Trim-Userid: 1000' -H 'X-Trim-Isadmin: true' "$url/state" >/dev/null 2>&1; then echo 'Package user reached the trusted gateway socket' >&2; exit 1; fi
curl --fail --silent --unix-socket "$socket" -H 'X-Trim-Userid: 1000' -H 'X-Trim-Isadmin: true' -H 'X-SwitchHosts-Request: 1' -H 'Content-Type: application/json' --data '{"revision":0,"entry":{"id":"smoke","title":"Smoke","enabled":true,"content":"10.0.0.8 smoke.test"}}' "$url/entry"
grep -q 'smoke.test' /etc/hosts
grep -q 'preserved.test' /etc/hosts
[ "$(stat -c '%u' "/proc/$web_pid")" = 65534 ]
[ "$(stat -c '%u' "/proc/$writer_pid")" = 0 ]
curl --fail --silent --unix-socket "$socket" -H 'X-Trim-Userid: 1000' -H 'X-Trim-Isadmin: true' "$url/state" > "$test_root/result.json"
# Repeated start and a full stop/start must preserve state and data ownership.
bash "$lifecycle" start
read -r same_pid _ < /run/switchhosts-fnos/web.pid
[ "$same_pid" = "$web_pid" ]
bash "$lifecycle" stop
[ ! -e "$socket" ]
[ ! -e /run/switchhosts-fnos/writer.sock ]
bash "$lifecycle" start
curl --fail --silent --unix-socket "$socket" -H 'X-Trim-Userid: 1000' -H 'X-Trim-Isadmin: true' "$url/state" > "$test_root/restarted.json"
grep -q 'smoke.test' "$test_root/restarted.json"
printf '\nLifecycle permissions, production sockets, privilege separation, hosts apply, and restart smoke test passed.\n'
