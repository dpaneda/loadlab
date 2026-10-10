# Start and stop the workload

## POST /workload/start

Sets `running=true`.

## POST /workload/stop

Sets `running=false`. The controller then converges the generated memory arena to zero.

