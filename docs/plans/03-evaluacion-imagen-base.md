# Evaluación — imagen base alternativa

## Contexto

Tras el paso 2, el arranque de sesión quedó en ~14 s, de los que ~9 s son el arranque del
invitado. La vía obvia era podar servicios de Ubuntu Server. La alternativa, que resultó mejor,
es no partir de Ubuntu Server: la imagen cloud estándar trae mucho que un sandbox desechable no
usa.

Para poder comparar bases sin tocar código se añadió `GELI_BASE_IMAGE`, que apunta
`--build-image` a otra imagen cloud.

## Resultados

Todas las golden se construyeron con la misma receta (Node 22, git, python3, claude-code) y se
midieron con el mismo comando.

| | Ubuntu Server 24.04 | **Ubuntu 24.04 minimal** | Debian 13 genericcloud |
|---|---|---|---|
| Imagen base | 596 MB | **254 MB** | 326 MB |
| Arranque kernel | 2,366 s | **0,766 s** | — |
| Arranque userspace | 5,208 s | 5,893 s | — |
| Arranque total | 7,575 s | **6,660 s** | — |
| Comando empieza a los | 8,43 s | **7,09 s** | — |
| Total de sesión | 14,08 s | **12,02 s** | 10,62 s * |
| Servicios corriendo | 20 | **14** | — |
| Scripts MOTD | 13 | **7** | — |
| Montajes 9p | sí | sí | **no** |

\* Debian parecía la más rápida porque no montaba nada.

## Conclusiones

**Ubuntu minimal es la mejor de las probadas**, y el ahorro viene del kernel: 2,366 s → 0,766 s,
**1,6 s** de los 2 s totales ganados. El initrd minimal trae muchos menos módulos.

Esto importa metodológicamente: `systemd-analyze blame` solo mide espacio de usuario, así que
ninguna poda de servicios habría encontrado ese 1,6 s. Verificado dentro de la VM: Node 22.23.3,
Claude Code 2.1.289, git y los tres montajes 9p funcionando.

**Debian 13 genericcloud queda descartada.** Su kernel «cloud» está recortado y no incluye los
módulos 9p:

```
mount: /workspace/geli-boot: unknown filesystem type '9p'
```

Sin 9p no hay workspace, que es la razón de ser de geli. Queda pendiente probar
`debian-13-generic` (sin el sufijo «cloud»), que usa kernel completo y sí debería traerlos.

## Alpine 3.22 — evaluada, descartada

Se evaluó después, por petición. Las suposiciones previas resultaron equivocadas en ambas
direcciones.

**Lo que funciona, contra lo esperado:**

- **9p funciona.** El kernel `-virt` trae los módulos; montó y leyó un fichero del host. Era el
  bloqueo que mató a Debian.
- **cloud-init funciona**, con el mismo patrón `write_files` + `runcmd`.
- **Claude Code corre sobre musl.** `apk add nodejs npm` da Node 22.23.2 (cumple el `>=22`),
  `npm install -g @anthropic-ai/claude-code` sale con exit 0 y `claude --version` responde
  `2.1.289`. No empaqueta binarios enlazados a glibc.

**Lo que no funciona, también contra lo esperado — es más lenta:**

| | uptime al comando |
|---|---|
| Alpine sin tocar | **16,8 s** |
| Alpine con red estática y sin chronyd | 6,5-7,9 s |
| Ubuntu minimal sin tocar | 7,09 s |

De fábrica tarda **2,4 veces más que Ubuntu minimal**. La causa no es la distribución: los módulos
de cloud-init suman 0,7 s y el kernel termina a los 2,8 s. Son `dhcpcd` negociando DHCP y
`chronyd` ajustando el reloj (4 s de *slew*).

Afinada con red estática iguala a Ubuntu minimal **sin afinar**, no la supera. Y el precio es una
segunda vía de programación del invitado: receta `apk` en vez de `apt`, autologin por
`/etc/inittab` con busybox getty en vez del drop-in de systemd, y musl como riesgo a futuro para
cualquier módulo npm nativo que el agente quiera compilar en un proyecto.

**Conclusión: no compensa.** Ubuntu minimal da casi el mismo arranque sin añadir una segunda
receta que mantener. La palanca que de verdad queda —red estática en vez de DHCP— aplica igual a
Ubuntu, donde `systemd-networkd-wait-online` cuesta 1,8 s.

Nota: la configuración estática que probé dejó el DNS sin salida (`DNS_OK=no`). Si alguna vez se
retoma Alpine, hay que resolver eso.

## Pendiente

Ortogonal a la base elegida, y acumulable:

- Ubuntu minimal **sigue trayendo snapd** (14 unidades) y 7 scripts de MOTD.
- `systemd-networkd-wait-online` sigue costando ~1,8 s en la cadena crítica.
- El arranque pasa por SeaBIOS → iPXE → GRUB antes de tocar el kernel. Un arranque directo
  (`-kernel`/`-initrd`) se los salta enteros, y es la palanca mayor que queda.
