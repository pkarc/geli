# Roughly a fifth of outbound connections on a given network can time out or drop mid-TLS, and
# `set -e` turns any one of them into a failed build. Every network step gets retries.
#
# Substituted into both the base recipe and every agent layer, so the two cannot drift: an agent's
# `install` field is documented as having `retry` in scope, and this is what puts it there.
retry() {
  n=0
  until [ "$n" -ge 5 ]; do
    "$@" && return 0
    n=$((n + 1))
    echo "geli: network step failed, retry $n/5: $*"
    sleep 3
  done
  return 1
}
