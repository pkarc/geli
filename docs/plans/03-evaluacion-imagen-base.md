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

**Alpine no se evaluó.** Es la más pequeña (178 MB), pero usa OpenRC en vez de systemd y musl en
vez de glibc: habría que reescribir la capa de programación del invitado (drop-in de autologin,
getty serie) y verificar Node sobre musl. Solo merece la pena si los números de las otras no
bastan.

## Pendiente

Ortogonal a la base elegida, y acumulable:

- Ubuntu minimal **sigue trayendo snapd** (14 unidades) y 7 scripts de MOTD.
- `systemd-networkd-wait-online` sigue costando ~1,8 s en la cadena crítica.
- El arranque pasa por SeaBIOS → iPXE → GRUB antes de tocar el kernel. Un arranque directo
  (`-kernel`/`-initrd`) se los salta enteros, y es la palanca mayor que queda.
