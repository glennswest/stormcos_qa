#!/bin/sh
# gather collector (ironprom): pod/StatefulSet state plus the live TSDB, targets,
# and buildinfo snapshot from the ironprom HTTP API.
API="${QA_API:-http://127.0.0.1:6443}"

$QA_SSH "
echo '== ironprom pod / statefulset =='
wget -qO- '$API/api/v1/namespaces/monitoring/pods' 2>/dev/null | tr ',' '\n' | grep -Ei 'ironprom|\"phase\"|\"ready\"|podIP' | head -40
echo
ip=\$(wget -qO- '$API/api/v1/namespaces/monitoring/pods' 2>/dev/null | grep -oE '\"podIP\":\"[0-9.]+\"' | head -1 | grep -oE '[0-9.]+')
EP=\"http://\${ip:-127.0.0.1}:9090\"
echo \"== ironprom endpoint: \$EP ==\"
echo '== buildinfo =='   ; wget -qO- \"\$EP/api/v1/status/buildinfo\" 2>/dev/null
echo; echo '== status/tsdb ==' ; wget -qO- \"\$EP/api/v1/status/tsdb\" 2>/dev/null
echo; echo '== status/runtimeinfo ==' ; wget -qO- \"\$EP/api/v1/status/runtimeinfo\" 2>/dev/null
echo; echo '== targets ==' ; wget -qO- \"\$EP/api/v1/targets\" 2>/dev/null
echo; echo '== rules ==' ; wget -qO- \"\$EP/api/v1/rules\" 2>/dev/null
echo; echo '== self-metrics (ironprom_*) ==' ; wget -qO- \"\$EP/metrics\" 2>/dev/null | grep '^ironprom_'
" 2>&1
