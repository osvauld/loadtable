# Cgroup delegation for `tasks2d` (worker and laptop)

Design §9 "Attribution of stalls". With a delegated cgroup subtree the daemon puts every job
in its own cgroup (`tasks.slice/job-<id>`), plus `tasks.slice/transfer` for sync work, and reads
exact disk bytes, peak memory, CPU time and stall times from it. Without delegation the daemon
still works: it keeps process groups, `tasks2 status` says
`attribution: process tree (no cgroup delegation: <reason>)`, and the per-job figures are summed
over the job's process tree (approximate, and no stall times).

You apply the steps below; the daemon never edits units or NixOS configuration. Nothing here
needs the daemon to run as root.

## What the daemon needs

1. `Delegate=yes` on the `tasks2d` user service, so systemd hands the service's cgroup to it.
   `tasks2d install` now renders this (and `DelegateSubgroup=daemon`, which starts the daemon
   in its own `daemon` leaf; systemd older than 254 ignores that line with a warning).
2. The **user manager** must itself own the controllers `io`, `memory` and `cpu`. Most distributions
   delegate only `pids memory cpu` to `user@.service`, so **`io` is missing by default**; without it
   the figures have no disk bytes and no I/O stall times (memory and CPU still work).
3. cgroup v2 mounted at `/sys/fs/cgroup` (the default on current NixOS and Arch).

The daemon finds its own cgroup from `/proc/self/cgroup`, creates `tasks.slice` and
`tasks.slice/transfer` in it, moves itself into the `daemon` leaf (a cgroup that holds processes
cannot enable controllers for its children), and enables `+io +memory +cpu` as far as they are
offered. Whatever it cannot do is the reason on the `attribution:` line.

## NixOS worker (192.168.1.7)

Root steps, in `nix/taskworker-host.nix` (or any module of the worker's configuration):

```nix
# let the user manager own the io controller too (default is "pids memory cpu")
systemd.services."user@".serviceConfig.Delegate = "cpu cpuset io memory pids";
```

Then the user service. If the worker runs `tasks2d` as a systemd *user* service defined in the
NixOS configuration:

```nix
systemd.user.services.tasks2d.serviceConfig = {
  Delegate = "yes";
  DelegateSubgroup = "daemon";   # systemd >= 254; harmless warning before that
};
```

If the unit comes from `tasks2d install` instead, delete the old
`~/.config/systemd/user/tasks2d.service` first (install never overwrites a different file) and run
`tasks2d install` again, or add the two lines to the `[Service]` section by hand.

Apply and restart:

```sh
sudo nixos-rebuild switch
systemctl --user daemon-reload
systemctl --user restart tasks2d     # only when no job is running: jobs do not survive a restart
```

A reboot or `loginctl terminate-user $USER` + login is needed once for a changed `user@.service`
delegation to take effect for the already running user manager.

## Laptop (Arch)

Same two pieces, as files:

```sh
# 1. the user manager owns io (root)
sudo mkdir -p /etc/systemd/system/user@.service.d
printf '[Service]\nDelegate=cpu cpuset io memory pids\n' | sudo tee /etc/systemd/system/user@.service.d/delegate.conf
sudo systemctl daemon-reload            # takes effect at the next login of the user manager

# 2. the tasks2d unit
mv ~/.config/systemd/user/tasks2d.service ~/.config/systemd/user/tasks2d.service.old   # install won't overwrite
tasks2d install                         # renders Delegate=yes and DelegateSubgroup=daemon
systemctl --user daemon-reload
```

Restart `tasks2d` when idle (`tasks2 drain`, wait for running jobs, then
`systemctl --user restart tasks2d`).

## How to verify

```sh
# the unit asks for delegation
systemctl --user show tasks2d -p Delegate -p DelegateSubgroup
#   Delegate=yes            DelegateSubgroup=daemon

# the user manager owns io, memory and cpu
cat /sys/fs/cgroup/user.slice/user-$(id -u).slice/user@$(id -u).service/cgroup.controllers
#   must list: cpu io memory pids (cpuset is optional)

# the service's cgroup and what the daemon enabled in it
CG=$(systemctl --user show tasks2d -p ControlGroup --value)
cat /sys/fs/cgroup$CG/cgroup.controllers
cat /sys/fs/cgroup$CG/cgroup.subtree_control      # +io +memory +cpu as "cpu io memory"
ls /sys/fs/cgroup$CG                              # daemon/  tasks.slice/  (tasks.slice/transfer)
cat /sys/fs/cgroup$CG/tasks.slice/cgroup.subtree_control

# the daemon's own verdict
tasks2 status | grep attribution
#   attribution: exact cgroup (/sys/fs/cgroup/.../tasks.slice; controllers io,memory,cpu)
```

While a job runs, `ls /sys/fs/cgroup$CG/tasks.slice` shows `job-<id>`; `cat .../job-<id>/io.stat`
shows its disk bytes. The directory disappears when the job ends.

If `attribution:` says `process tree`, the reason follows it, for example:

| reason | meaning | fix |
|---|---|---|
| `io error on .../tasks.slice: Permission denied` | the service cgroup is not delegated | `Delegate=yes` missing, or the service was not restarted |
| `cgroup v2 is not mounted at /sys/fs/cgroup` | cgroup v1 or a container without the mount | none here; the fallback is fine |
| `controllers io` missing (status shows `controllers memory,cpu`) | `user@.service` does not delegate `io` | the `user@` step above, then re-login |

## What changes for running jobs

- `KillMode` is unchanged (`control-group`): stopping the service stops everything in its cgroup,
  as before. Jobs never survived a daemon restart as running jobs; they are marked interrupted.
- At start the daemon removes job cgroups left by a previous run (killing what is still in them).
- Without `Delegate=yes` nothing else changes: jobs run in process groups exactly as before.
