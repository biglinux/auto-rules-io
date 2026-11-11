# auto-rules-io
I/O, memory and boot tuning for BigLinux

## I/O scheduler / Agendador de E/S

EN - Automatically chooses the I/O scheduler: BFQ on rotational disks and eMMC, Kyber on SSD and NVMe.

PT - Escolhe automaticamente o agendador de E/S: BFQ em discos rotacionais e eMMC, Kyber em SSD e NVMe.

## Disk latency target for the session / Meta de latência de disco da sessão

EN - The session keeps a 2 ms disk latency target on the disks of `/` and `/home` (`IODeviceLatencyTargetSec=` on `user.slice`, and on `app.slice` and `session.slice` in the user manager, which `Delegate=io` on `user@.service` allows). While programs miss it, the kernel's io.latency controller throttles everything beside them: `system.slice` and `machine.slice` (system services, containers and virtual machines, including work done for the user such as package updates and Docker builds) and the user's `background.slice` (indexers). While the session does no I/O, they run at full speed. This works under any scheduler: ionice classes act only inside one cgroup under BFQ and not at all under Kyber, and `IOWeight` shares throughput under BFQ but bounds no latency.

PT - A sessão mantém uma meta de latência de 2 ms nos discos de `/` e `/home` (`IODeviceLatencyTargetSec=` em `user.slice`, e em `app.slice` e `session.slice` no gerenciador do usuário, o que o `Delegate=io` no `user@.service` permite). Enquanto os programas passam dessa meta, o controlador io.latency do kernel freia tudo o que roda ao lado deles: `system.slice` e `machine.slice` (serviços do sistema, contêineres e máquinas virtuais, inclusive trabalho feito para o usuário, como atualizações de pacotes e builds do Docker) e a `background.slice` do usuário (indexadores). Enquanto a sessão não faz E/S, eles rodam na velocidade máxima. Funciona com qualquer agendador: as classes do ionice só valem dentro de um cgroup no BFQ e não valem no Kyber, e o `IOWeight` divide a vazão no BFQ, mas não limita a latência.

EN - Measured with a sequential reader in the background (cold start, seconds). Alone, LibreOffice and Firefox start in 0.8 and 1.4 s on the NVMe, 10.6 and 16.5 s on the HDD.

PT - Medido com um leitor sequencial em segundo plano (início a frio, segundos). Sozinhos, LibreOffice e Firefox abrem em 0,8 e 1,4 s no NVMe e em 10,6 e 16,5 s no HD.

| Disk | Target | LibreOffice | Firefox |
|---|---|---|---|
| NVMe | none | 16.5–45 | 65–71 |
| NVMe | 2 ms | 1.5–2.4 | 2.1–3.2 |
| HDD | none | 104–111 | 112–132 |
| HDD | 2 ms | 37–40 | 74–80 |

EN - On the NVMe, a reader that saturates the disk loses about a quarter of its rate while the session does any I/O; buffered writes and random reads lose nothing measurable, and nothing measurable is lost on the HDD. On an HDD the target does not help against buffered writes, whose writeback it does not control.

PT - No NVMe, um leitor que satura o disco perde cerca de um quarto da vazão enquanto a sessão faz qualquer E/S; escritas em buffer e leituras aleatórias não perdem nada mensurável, e no HD nada mensurável se perde. No HD a meta não ajuda contra escritas em buffer, cujo writeback ela não controla.

EN - The target was measured on plain partitions. systemd resolves a LUKS volume, or an LVM volume on one disk, to the disk under it. On RAID, LVM over several disks, multi-device Btrfs or a network `/home` the target cannot reach the disks and does not help.

PT - A meta foi medida em partições comuns. O systemd resolve um volume LUKS, ou um volume LVM sobre um disco só, para o disco que está por baixo. Em RAID, LVM sobre vários discos, Btrfs com vários discos ou `/home` em rede, a meta não alcança os discos e não ajuda.

EN - `user.slice` holds every login, SSH and root included. On a server, an administrator's shell would throttle databases, containers and VMs. To turn the target off, mask the three drop-ins and reboot:

PT - A `user.slice` reúne todos os logins, inclusive SSH e root. Num servidor, o shell de um administrador frearia bancos de dados, contêineres e VMs. Para desligar a meta, mascare os três drop-ins e reinicie:

```bash
sudo mkdir -p /etc/systemd/system/user.slice.d /etc/systemd/user/app.slice.d /etc/systemd/user/session.slice.d
sudo ln -s /dev/null /etc/systemd/system/user.slice.d/50-io-latency.conf
sudo ln -s /dev/null /etc/systemd/user/app.slice.d/50-io-latency.conf
sudo ln -s /dev/null /etc/systemd/user/session.slice.d/50-io-latency.conf
```

EN - Removing the target takes effect at the next boot: systemd drops the setting but leaves the kernel's target in place. Clearing it by hand (`target=0` in `io.latency`) while it throttles leaves the throttled groups delayed until reboot, since the kernel clears the delay only on the group that had the target.

PT - Remover a meta só vale no próximo boot: o systemd descarta a configuração, mas deixa a meta no kernel. Zerá-la à mão (`target=0` em `io.latency`) enquanto ela freia deixa os grupos freados atrasados até reiniciar, porque o kernel só limpa o atraso do grupo que tinha a meta.

## Linker cache at boot / Cache do ligador no boot

EN - `ldconfig.service` rebuilds `/etc/ld.so.cache` at boot only when it is missing or empty, instead of at the first boot after every update. pacman already runs ldconfig after each transaction, so the boot rebuild repeated it inside `sysinit.target`: 8.8–9.9 s cold on an HDD, 0.5 s on an NVMe. Libraries added without pacman (a `systemd-sysext` image, a transaction killed before its end) need `sudo ldconfig`.

PT - O `ldconfig.service` refaz o `/etc/ld.so.cache` no boot só quando ele falta ou está vazio, em vez de no primeiro boot depois de cada atualização. O pacman já roda o ldconfig a cada transação, então o boot repetia o trabalho dentro do `sysinit.target`: 8,8–9,9 s a frio num HD, 0,5 s num NVMe. Bibliotecas adicionadas sem o pacman (uma imagem do `systemd-sysext`, uma transação interrompida antes do fim) precisam de `sudo ldconfig`.

## snap at login / snap no login

EN - snapd's autostart (`snap userd --autostart`) starts only where snapd is enabled (`/run/snapd.socket` exists). With snapd turned off it ran at every login: 3.3–4.2 s cold on an HDD.

PT - O autostart do snapd (`snap userd --autostart`) só inicia onde o snapd está habilitado (existe `/run/snapd.socket`). Com o snapd desligado, ele rodava em todo login: 3,3–4,2 s a frio num HD.
