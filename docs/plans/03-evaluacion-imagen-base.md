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

**Ubuntu minimal fue la mejor de las probadas inicialmente**, y el ahorro viene del kernel: 2,366 s → 0,766 s,
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

## Alpine 3.22 — adoptada

Se evaluó después, por petición. Las suposiciones previas resultaron equivocadas en ambas
direcciones.

**Lo que funciona, contra lo esperado:**

- **9p funciona.** El kernel `-virt` trae los módulos; montó y leyó un fichero del host. Era el
  bloqueo que mató a Debian.
- **cloud-init funciona**, con el mismo patrón `write_files` + `runcmd`.
- **Claude Code corre sobre musl.** `apk add nodejs npm` da Node 22.23.2 (cumple el `>=22`),
  `npm install -g @anthropic-ai/claude-code` sale con exit 0 y `claude --version` responde
  `2.1.289`. No empaqueta binarios enlazados a glibc.

**musl, medido en vez de supuesto.** El riesgo real no era Claude Code sino las dependencias de
los proyectos del usuario. Nueve paquetes con binarios nativos —`esbuild`, `sharp`,
`better-sqlite3`, `bcrypt`, `numpy`, `pandas`, `cryptography`, `lxml`, `psycopg2`— instalan desde
ruedas `musllinux` y prebuilds musl, **cargan y ejecutan**, en una imagen **sin gcc ni make**.
Ninguno compiló desde fuente. Queda sin cubrir la cola larga: paquetes de nicho o ruedas internas
de empresa.

**Resultado final, ya portada:**

| | Ubuntu Server | Alpine |
|---|---|---|
| Sesión completa | 13,3-15,1 s | **13,7-14,0 s** |
| Arranque hasta el comando | 8,43 s | **7,35 s** |
| Footprint (base + golden) | 1,9 GB | **656 MB** |
| Kernel | 6.8 | **6.12** |

Empata en tiempo y ocupa un tercio.

### Tres trampas del porte

Ninguna era de la distribución; las tres eran configuración, y las tres fallaban en silencio.

1. **cloud-init no puede fijar un `uid` en Alpine** y falla el módulo entero al pedírselo. Sin
   usuario → fallan los `write_files` con `owner` → no se escribe `mounts.sh` → no se monta el
   workspace. El usuario se crea ahora en el script de provisión.
2. **El uid debe coincidir con el del host.** El usuario `alpine` ocupa el 1000 y empuja a
   `sandbox` al 1001: con eso el agente lee el proyecto por 9p pero no puede escribir. Se elimina
   `alpine` para liberar el uid.
3. **La imagen trae un menú de arranque SYSLINUX de 10 s.** No aparece en `uptime` ni en ninguna
   medición desde dentro del invitado, solo en tiempo de reloj — parecía sobrecoste de QEMU, y
   explicaba él solo toda la diferencia aparente con Ubuntu. Ahora `TIMEOUT 1`.

El patrón del punto 3 se repite en esta evaluación: `systemd-analyze` solo ve espacio de usuario y
`uptime` solo cuenta desde que arranca el kernel. **Medir desde dentro del invitado no ve ni el
gestor de arranque ni el initrd**, que es donde estaba buena parte del tiempo en los dos casos.

### Nota sobre el método

Alpine se descartó dos veces por razonamiento antes de medirla. Los tres argumentos en contra
—segunda receta, autologin por inittab, musl— cayeron al probarlos. El de la "segunda receta"
nunca fue real: es una receta en cualquier caso, solo que distinta.

## Pendiente

Sobre la base adoptada:

- El arranque pasa por SeaBIOS → iPXE → SYSLINUX antes de tocar el kernel. Un arranque directo
  (`-kernel`/`-initrd`) se los salta enteros, y es la palanca mayor que queda.
- La ROM de arranque por red de iPXE añade tiempo de BIOS y no se usa: `romfile=` la quita.
- Los 4 GB de RAM asignados están holgados — el invitado usa 476 MB.
