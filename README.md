# auto-rules-io

**I/O, memory and boot tuning for [BigLinux](https://www.biglinux.com.br).**

Your laptop has one disk, and everybody wants it at the same time: the indexer, the package manager, the backup, the browser you just clicked on. Guess which one you are waiting for. This package makes sure it is the browser that gets through, picks a sane I/O scheduler for each disk, and stops the boot from redoing work pacman already did.

No GUI, no "AI-powered optimizer". Udev rules, systemd drop-ins, sysctl files, two short shell scripts and one small user service, `deskbalance`, that sleeps until a program starts or waits on the disk.

## Contents

- [What's in the box](#whats-in-the-box)
- [Install](#install)
- [I/O scheduler](#io-scheduler)
- [Disk latency target for the session](#disk-latency-target-for-the-session)
- [deskbalance](#deskbalance)
- [Faster boot and login](#faster-boot-and-login)
- [Memory and swap](#memory-and-swap)
- [Btrfs housekeeping](#btrfs-housekeeping)
- [Odds and ends](#odds-and-ends)
- [License](#license)

## What's in the box

| File | What it does |
|---|---|
| `etc/udev/rules.d/60-ioschedulers.rules` | Picks the I/O scheduler per disk type; sets APM level 127 on rotational disks |
| `usr/lib/systemd/system/user.slice.d/50-io-latency.conf` | 2 ms disk latency target for every login session |
| `usr/lib/systemd/system/user@.service.d/50-io-latency.conf` | Hands the `io` controller to the user manager |
| `usr/lib/systemd/user/{app,session}.slice.d/50-io-latency.conf` | The same target inside the session |
| `usr/bin/deskbalance`, `usr/lib/systemd/user/deskbalance.service`, `usr/lib/systemd/user.conf.d/50-deskbalance.conf` | Moves the job hogging the disk out of the way when a program starts or waits |
| `usr/lib/systemd/system/ldconfig.service.d/50-pacman.conf` | Rebuilds the linker cache at boot only when it is missing |
| `usr/lib/systemd/user/app-snap\x2duserd\x2dautostart@autostart.service.d/50-snapd.conf` | Runs snapd's login helper only where snapd is enabled |
| `etc/sysctl.d/91-memory.conf` | Swap, cache and writeback settings tuned for zram |
| `etc/sysctl.d/99-games-tweaks.conf` | Raises `vm.max_map_count` for games and turns off core dumps |
| `etc/modprobe.d/disable_snd_itco.conf` | Blacklists the Intel chipset watchdog and the PC speaker |
| `usr/bin/big-btrfs-fix-to-timeshift`, `usr/bin/big-btrfs-balance`, `usr/lib/systemd/system/big-btrfs-balance.service` | Resets the Btrfs default subvolume and runs a light balance after boot |

## Install

It ships in the BigLinux repositories:

```bash
sudo pacman -S auto-rules-io
```

pacman reloads systemd on install, so the drop-ins are picked up right away; the ldconfig and snapd ones only matter at the next boot and login anyway. The scheduler rules and sysctl files live in `/etc`, where pacman's hooks do not look, so they apply at the next boot, or now with:

```bash
sudo udevadm trigger -c change && sudo sysctl --system
```

The module blacklist waits for the reboot.

The install script enables `big-btrfs-balance.service`, and on every upgrade it rewrites one specific set of old Btrfs mount options in `/etc/fstab` (`,space_cache=v2,autodefrag,compress-force=zstd:2,commit=15`, with or without `,discard=async,ssd`) to `,compress-force=zstd`. Other fstab lines are left alone.

## I/O scheduler

| Disk | Scheduler |
|---|---|
| Rotational `sd*` (HDD, USB HDD included) | BFQ |
| Non-rotational `sd*` (SATA or USB SSD) | Kyber |
| `mmcblk*` (eMMC, SD card) | BFQ |
| `nvme*` | Kyber |

Anything else (`vd*`, a 27th `sd` disk) keeps the kernel's default. Rotational disks also get `hdparm -B 127`, the highest APM level that still lets the disk spin down, provided `hdparm` is installed.

## Disk latency target for the session

### What it does

Every login session gets a 2 ms disk latency target on the disks holding `/` and `/home` (`IODeviceLatencyTargetSec=` on `user.slice`, and on `app.slice` and `session.slice` inside the user manager). While your programs miss it, the kernel's io.latency controller throttles everything around them:

- `system.slice` and `machine.slice`: system services, containers and VMs, including work done on your behalf such as package updates and Docker builds;
- the user's `background.slice`: indexers and other polite-in-theory background jobs.

When the session does no I/O, nothing is throttled and everyone runs at full speed. It works under any scheduler, which is the point: ionice classes only act inside one cgroup under BFQ and not at all under Kyber, and `IOWeight` shares throughput under BFQ but promises nothing about latency.

2 ms is not magic, just measured: 1 ms is slower on HDDs and 3 ms is slower on NVMe.

### Numbers

Cold start in seconds, with a sequential reader hammering the disk in the background:

| Disk | Target | LibreOffice | Firefox |
|---|---|---|---|
| NVMe | none | 16.5–45 | 65–71 |
| NVMe | 2 ms | 1.5–2.4 | 2.1–3.2 |
| HDD | none | 104–111 | 112–132 |
| HDD | 2 ms | 37–40 | 74–80 |

For reference, alone they start in 0.8 and 1.4 s on the NVMe, and 10.6 and 16.5 s on the HDD. The HDD is still an HDD; this package has limits. The NVMe is a Ryzen 7 5700U laptop under Kyber, the HDD an i3-2375M under BFQ, with the page cache dropped before every start. One machine each, so your mileage will vary.

### The fine print

- **The bill.** On the NVMe, a reader that saturates the disk loses about a quarter of its rate while the session does any I/O. Buffered writes and random reads lose nothing measurable, and nothing measurable is lost on the HDD.
- **Writeback.** On an HDD the target does not help against buffered writes: their writeback happens outside its control.
- **Your background counts too.** All I/O from the session counts toward the `user.slice` target, `background.slice` included. While an indexer such as Baloo or localsearch misses the target, system services and VMs are throttled even if no program waits. The kernel adds each request's latency to every ancestor group with a target, and `background.slice` lives inside `user.slice`.
- **Storage it can reach.** The target was measured on plain partitions. systemd resolves a LUKS volume, or an LVM volume on a single disk, to the disk under it. On RAID, LVM over several disks, multi-device Btrfs or a network `/home`, the target cannot reach the disks and does nothing.

### Turning it off

`user.slice` holds every login, SSH and root included. On a server, that means an administrator's `pacman -Syu` would throttle the databases and VMs, which is rarely what anyone wants at 3 AM. Mask the three targets and reboot (the `Delegate=io` drop-in is harmless on its own):

```bash
sudo mkdir -p /etc/systemd/system/user.slice.d /etc/systemd/user/app.slice.d /etc/systemd/user/session.slice.d
sudo ln -s /dev/null /etc/systemd/system/user.slice.d/50-io-latency.conf
sudo ln -s /dev/null /etc/systemd/user/app.slice.d/50-io-latency.conf
sudo ln -s /dev/null /etc/systemd/user/session.slice.d/50-io-latency.conf
```

The reboot is not optional: systemd drops the setting but leaves the kernel's target in place. Clearing it by hand (`target=0` in `io.latency`) while it throttles leaves the throttled groups delayed until reboot anyway, because the kernel only clears the delay on the group that had the target.

## deskbalance

The latency target throttles `background.slice`, but a copy started from your file manager or a terminal lands in `app.slice`, right next to the browser you are waiting for, where the target cannot tell them apart. `deskbalance` is a user service that fixes that:

- **A program starts** (a unit appears in `app.slice`): for the 10 s after the last start (30 s at most, then none for 10 s), `background.slice` and any `app.slice` unit burning more than an eighth of the CPUs (at least one) share a quarter of the CPUs (at least one), so the new program keeps whole cores.
- **A job hogs the disk** (at a start, or whenever `app.slice` waits on the disk): the busiest job of a program scope that has run for a minute or more (younger ones may still be loading) moves to a scope of its own in `background.slice`, under the latency target, with room for what it allocates plus 256 MB of page cache, so a big copy cannot push your programs out of RAM. "Job" means process group: the `cp` you started from a terminal moves, the terminal stays.
- **The job goes quiet** for 10 s: it goes back to `app.slice`, in a scope named after the one it left.

It blocks in inotify, a PSI trigger and a signalfd, so it does not wake up while nothing starts and nothing waits on the disk; while a job is moved, it looks at it once a second. On SIGTERM it lifts the CPU caps and puts every moved job back, and an instance that replaces a killed one takes over what it left. On a kernel without unprivileged PSI triggers, jobs move only when a program starts.

Measured on a laptop with an HDD, with a 4 GB copy started as a job from a terminal (cold start, seconds):

| | LibreOffice | Firefox |
|---|---|---|
| without deskbalance | 86–95 | 81–89 |
| with deskbalance | 13–32 | 21–24 |

LibreOffice takes the high end when it is the first program to start after the copy: the copy then moves as LibreOffice starts, not before.

At rest it uses 0.6 MB of memory and no wakeups.

The fine print:

- A program that does the heavy I/O itself (a browser download, a game updater) moves as a whole, and comes back in a scope with `-deskbalance-…` added to its name.
- Any unit that appears in `app.slice` counts as a program starting, D-Bus-activated and timer-started ones included.
- It does not know which window has the focus, so the program you are using can be the one capped for 10 s if it burns a lot of CPU while another one starts.
- On Plasma, `krunner`, the polkit agent and the kscreen OSD run in `background.slice`, so they share that quarter of the CPUs for the 10 s after a program starts.
- It reads each program's disk bytes from its cgroup, so the package turns on I/O accounting for user units (`DefaultIOAccounting=yes`), which applies as soon as the install reloads each user's systemd instance. deskbalance itself starts when that instance starts again: at the next login with no other session of that user open and lingering off, or after a reboot. It caps busy `app.slice` units only where something there already enabled the CPU controller; otherwise only `background.slice` is capped, and its log says so.
- The package enables it for every user when it first installs it. To turn it off for everyone, on the same terms: `sudo systemctl --global disable deskbalance` (upgrades leave that alone). For yourself only: `systemctl --user mask --now deskbalance`, since a user cannot disable what is enabled globally.

## Faster boot and login

### Linker cache

`ldconfig.service` rebuilds `/etc/ld.so.cache` at boot only when the cache is missing or empty, instead of at the first boot after every update. pacman already runs ldconfig after each transaction, so the boot was redoing it inside `sysinit.target`, where everything else waits: 8.8–9.9 s cold on an HDD, 0.5 s on an NVMe.

If you add libraries without pacman (a `systemd-sysext` image, a transaction killed halfway), run `sudo ldconfig` yourself. You were going to anyway, right?

### snapd

snapd's XDG autostart (`snap userd --autostart`) only starts where snapd is enabled, that is, where `/run/snapd.socket` exists. With snapd turned off it still ran at every login and achieved nothing in 3.3–4.2 s cold on an HDD.

## Memory and swap

`91-memory.conf` assumes zram (or zswap) for swap:

| Setting | Value | Effect |
|---|---|---|
| `vm.page-cluster` | 0 | Swap in one page at a time; zram compresses pages individually |
| `vm.swappiness` | 30 | Prefer dropping cache over swapping |
| `vm.vfs_cache_pressure` | 75 | Keep directory and inode caches a little longer |
| `vm.watermark_scale_factor` | 150 | Wider gap between watermarks, so kswapd starts earlier |
| `vm.watermark_boost_factor` | 15000 | Kernel default, written down: boost watermarks after fragmentation events |
| `vm.dirty_background_ratio` / `vm.dirty_ratio` | 5 / 15 | Start writeback sooner |
| `vm.compaction_proactiveness` | 20 | Kernel default, written down: background compaction level |
| `vm.extfrag_threshold` | 300 | Compact rather than reclaim a bit sooner (default 500) |
| `vm.panic_on_oom` | 0 | Kernel default, written down: let the OOM killer pick a victim instead of panicking |

`vm.min_free_kbytes` is not here: the `pre-systemd-swap` service sets it to 3% of RAM, up to 512 MB.

## Btrfs housekeeping

`big-btrfs-balance.service` runs once per boot, after `multi-user.target`:

1. `big-btrfs-fix-to-timeshift` resets the default subvolume to the top-level one (the parent of `@`), which Timeshift expects.
2. Five minutes later, `big-btrfs-balance` runs `btrfs balance` with `-dusage=50 -musage=50` on `/`, and on `/home` if it is a different filesystem, under `nice 19` and `ionice -c 3` (the latter only counts on BFQ disks).

## Odds and ends

- `99-games-tweaks.conf` sets `vm.max_map_count` to 2147483642, the value SteamOS uses for games, and `kernel.core_pattern` to `|/bin/false`, which throws core dumps away, so `coredumpctl` (and any crash reporter built on it) has nothing to show. The `99-` prefix puts it last: sysctl files apply in name order, Manjaro's `10-manjaro.conf` sets `vm.max_map_count`, systemd's `50-coredump.conf` sets `kernel.core_pattern`, and the later name wins. If `systemd-coredumpd.service` is enabled, its `systemd-coredump-register.service` sets `kernel.core_pattern` again after sysctl runs, and core dumps come back.
- `disable_snd_itco.conf` blacklists `iTCO_wdt`, the Intel chipset watchdog, which nothing on the desktop arms, and `pcspkr`, the PC speaker, so nothing beeps at you from the motherboard.

## License

GPL, as declared in [`pkgbuild/PKGBUILD`](pkgbuild/PKGBUILD).
