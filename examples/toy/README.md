# toy

A fake project for trying canirun without a real test suite. Each script
burns a known amount of CPU so the learned weights are predictable.

```sh
export XDG_STATE_HOME=/tmp/canirun-toy XDG_CONFIG_HOME=/tmp/canirun-toy
mkdir -p /tmp/canirun-toy/canirun && cp config.toml /tmp/canirun-toy/canirun/
for i in 1 2 3 4 5 6; do canirun exec -- ./heavy.sh & done
canirun exec --front -- ./lint.sh &
canirun status
wait
canirun history
```
