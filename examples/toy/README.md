# toy

A fake project for trying task-traffic-controller without a real test suite. Each script
burns a known amount of CPU so the learned weights are predictable.

```sh
export XDG_STATE_HOME=/tmp/task-traffic-controller-toy XDG_CONFIG_HOME=/tmp/task-traffic-controller-toy
mkdir -p /tmp/task-traffic-controller-toy/task-traffic-controller && cp config.toml /tmp/task-traffic-controller-toy/task-traffic-controller/
for i in 1 2 3 4 5 6; do task-traffic-controller exec -- ./heavy.sh & done
task-traffic-controller exec --front -- ./lint.sh &
task-traffic-controller status
wait
task-traffic-controller history
```
