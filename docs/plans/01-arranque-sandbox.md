# Paso 1 — Hacer que el sandbox arranque y ejecute el comando

## Contexto

Hoy `geli` no funciona en ningún caso. Hay dos fallos encadenados en la generación del
cloud-init en `src/main.rs`:

1. **El YAML no parsea.** En `src/main.rs:320`, `mount_cmds.replace('\n', "\n  - ")` indenta
   las entradas de montaje con 2 espacios, mientras el resto de `runcmd` está a 10. Verificado:
   el parser falla con `expected '<document start>'`. Además `mount_cmds` termina en `\n`, lo
   que genera una entrada `- ` vacía. Como el directorio actual siempre se registra en el
   workspace, `mount_cmds` nunca está vacío → **falla siempre**.

2. **El comando nunca se ejecuta.** El comentario de `src/main.rs:293` dice que hay autologin en
   `ttyS0`, pero no se configura en ninguna parte. El comando se añade a `.bashrc`, que requiere
   una sesión de login que nunca ocurre. El bloque `users:` tampoco incluye `default`, así que
   elimina el usuario `ubuntu`, y `sandbox` queda con la contraseña bloqueada.

El síntoma combinado es el peor posible: la VM arranca con normalidad, cloud-init se cae en
silencio y el usuario se queda en una consola serie muerta.

**Resultado esperado:** `geli <cmd>` arranca la VM, monta los directorios del workspace, ejecuta
`<cmd>` de forma interactiva en el TTY y apaga limpiamente.

Fuera de alcance (pasos posteriores acordados): golden image, postura de red (se queda abierta a
propósito), KVM opcional.

## Enfoque

La causa raíz no es el indentado en sí: es **construir YAML con `format!` y `replace`**. El
arreglo estructural es que el YAML tenga forma fija y que toda la parte variable viva dentro de
bloques escalares (`content: |`), que solo requieren indentar un bloque de texto de forma uniforme.

Toda la lógica de montaje pasa a un script de shell escrito con `write_files`, invocado desde un
único `runcmd`. Así el YAML deja de depender del número de directorios.

### Cambios en `src/main.rs`

**a) Extraer funciones puras y testeables.** Es lo que hace posible testear esto sin arrancar una
VM. `execute_sandbox` tiene hoy 175 líneas y mezcla generación de configuración con ejecución de
procesos.

- `fn indent_block(text: &str, spaces: usize) -> String` — prefija cada línea no vacía. Es la
  única operación de indentado del programa.
- `fn build_mount_script(dirs: &[PathBuf], cur: &Path) -> (String, String)` — devuelve el cuerpo
  del script de montaje y el nombre de la carpeta activa. Reemplaza el bucle de `src/main.rs:268-288`
  (la construcción de `qemu_args` se queda donde está).
- `fn build_cloud_init(active: &str, cmd: &str, mounts: &str, keys: &Keys) -> String` — sustituye
  el `format!` de `src/main.rs:294-325`.

**b) Nueva forma del cloud-init.** Raw string **sin indentación global** (hoy todo está a 8
espacios, lo que es frágil):

```yaml
#cloud-config
users:
  - name: sandbox
    sudo: ALL=(ALL) NOPASSWD:ALL
    shell: /bin/bash

write_files:
  - path: /etc/systemd/system/serial-getty@ttyS0.service.d/autologin.conf
    content: |
      [Service]
      ExecStart=
      ExecStart=-/sbin/agetty --autologin sandbox --noclear %I $TERM
  - path: /etc/geli/env            # 0600, chown en runcmd
    content: |
      export ANTHROPIC_API_KEY="..."
      export OPENAI_API_KEY="..."
  - path: /etc/geli/mounts.sh
    content: |
      <indent_block(mounts, 6)>
  - path: /etc/geli/profile
    content: |
      [ -f ~/.bashrc ] && . ~/.bashrc
      . /etc/geli/env
      cd /workspace/<active>
      <cmd>
      sudo poweroff

runcmd:
  - [apt-get, update]
  - [apt-get, install, -y, nodejs, npm, python3, python3-pip]
  - [npm, install, -g, "@anthropic-ai/claude-code"]
  - bash /etc/geli/mounts.sh
  - install -o sandbox -g sandbox -m 0644 /etc/geli/profile /home/sandbox/.bash_profile
  - chown sandbox:sandbox /etc/geli/env && chmod 0600 /etc/geli/env
  - chown -R sandbox:sandbox /home/sandbox/.cache /workspace
  - systemctl daemon-reload && systemctl restart serial-getty@ttyS0.service
```

Tres detalles que importan:

- **`.bash_profile`, no `.bashrc`.** `.bashrc` se ejecuta en *toda* shell: si el agente lanza un
  subshell, vuelve a correr el comando y hace `poweroff` en medio de la sesión. `.bash_profile`
  solo corre en shells de login, que es exactamente lo que da el autologin de agetty.
- **Los ficheros se escriben en `/etc/geli/`, no en `/home/sandbox/`.** El módulo `write_files`
  de cloud-init corre *antes* que `users-groups`, así que escribir directo en el home crearía el
  directorio antes que el usuario y rompería el `skel`. Se copian con `install` desde `runcmd`,
  que sí corre después.
- **Las cachés npm/pip se montan dentro de `mounts.sh`**, antes de los `apt`/`npm`. Hoy se montan
  después del `npm install -g` (`src/main.rs:308-309`), lo que las deja inútiles. Esto no resuelve
  la lentitud del arranque — eso es el paso 2 — pero deja de ser gratuitamente incorrecto.

**c) Guarda para `active_folder_name`.** Si `canonicalize()` falla (`src/main.rs:272`), queda vacío
y se genera `cd /workspace/`. Fallback al `file_name()` del directorio actual.

**d) Permisos del directorio temporal.** `/tmp/sandbox-share-<pid>` (`src/main.rs:251`) contiene las
llaves de API en texto plano y hoy es legible por todo el mundo. `fs::set_permissions` a `0700`
justo después del `create_dir_all`. Es una línea; no resuelve el problema de fondo de inyectar la
llave en la VM, pero quita la exposición en el host.

**e) Arreglar los stubs que no compilan.** `src/main.rs:385` y `src/main.rs:390` usan `Vec` sin
parámetro de tipo → `Vec<PathBuf>`. No afecta a Linux, pero rompe el build en cuanto alguien
compile en macOS o Windows.

### Tests (`src/main.rs`, módulo `#[cfg(test)]`)

El bug del YAML lo habría cazado un único test de "esto parsea". Añadir:

- `cloud_init_parsea` — con 1, 2 y 0 directorios. Es el test que importa.
- `cloud_init_sin_lineas_vacias_en_runcmd` — captura el `- ` suelto del `\n` final.
- `indent_block_*` — líneas vacías no se rellenan de espacios.
- `build_mount_script_detecta_directorio_activo` — incluyendo el caso de fallback de (c).

Requiere un **dev-dependency** de YAML (`yaml-rust2`; `serde_yaml` está archivado), lo que implica
red para `cargo add`. Si no hay red, el fallback es un test estructural sin dependencias: todas las
líneas de `runcmd` comparten indentación, ningún ítem queda vacío, todo `content: |` está indentado
por debajo de su clave. Más débil, pero cubre esta clase de fallo.

## Verificación

1. `cargo test` — debe pasar, y los tests deben fallar si se revierte el arreglo de indentado
   (lo compruebo revirtiendo a mano antes de dar por bueno el test).
2. `cargo build --release`.
3. End-to-end en un directorio de prueba con un `.geli.json` desechable:
   `geli echo GELI_OK` → la VM arranca, monta `/workspace/<dir>`, imprime `GELI_OK` y apaga sola.
   Este arranque tarda varios minutos (apt + npm en cada invocación; es justo lo que arregla el
   paso 2).
4. Caso interactivo: `geli bash -i` para confirmar que el TTY responde y que un subshell anidado
   **no** dispara el `poweroff` — que es el fallo que evita el cambio a `.bash_profile`.
5. Durante la verificación mantengo el qcow2 y el directorio temporal sin borrar, para poder leer
   `/var/log/cloud-init-output.log` si algo falla. Se revierte al terminar.

Entorno ya confirmado en esta máquina: qemu-system-x86_64, qemu-img, genisoimage, `/dev/kvm` y la
imagen base en `~/qemu-sandbox/`.

## Riesgos

- **El autologin podría no aplicarse a tiempo.** El `restart` de `serial-getty@ttyS0` va al final
  de `runcmd`; si cloud-init tarda, el usuario ve una consola quieta un rato. Si molesta, se mueve
  a `bootcmd` (más temprano) en vez de `runcmd`.
- **Sin red para el dev-dependency**, el test de parseo baja a la variante estructural descrita.
- No he arrancado la VM todavía: los puntos 1 y 2 del contexto están verificados (el del YAML
  ejecutando el parser, el del login por lectura de código), pero el comportamiento real de
  cloud-init se confirma en el paso 3 de verificación.

---

## Resultado (verificado)

Implementado y verificado end-to-end en Linux con KVM.

**Hallazgo no previsto en el plan: el disco se llenaba.** El overlay qcow2 heredaba los 3.5 GiB de
la imagen base y `apt install nodejs npm` lo desbordaba (`No space left on device`), de modo que
`claude-code` nunca llegaba a instalarse. Se añadió `SANDBOX_DISK_SIZE = "20G"` como argumento de
`qemu-img create`; qcow2 es disperso y `growpart` expande la partición al arrancar.

Evidencia de la ejecución final:

| Comprobación | Resultado |
|---|---|
| `cargo test` | 7/7 (y `cloud_init_parses` falla si se revierte el indentado) |
| `cargo clippy --all-targets` | 0 warnings |
| Comando del usuario ejecutado | `GELI_OK_MARKER` |
| Montaje 9p del directorio activo | `marker file` leído desde `/workspace/geli-e2e` |
| `claude-code` instalado | `/usr/local/bin/claude`, `added 3 packages` |
| Comando en `.bash_profile`, no `.bashrc` | `CHECK_BASHRC=0`, `CHECK_PROFILE=1` |
| Subshell anidado no dispara poweroff | `CHECK_NESTED_OK` + `CHECK_AFTER_NESTED` |
| TTY real en el invitado | `CHECK_TTY_OK` |
| Directorio de trabajo | `CHECK_PWD=/workspace/geli-e2e` |
| Apagado y limpieza | `reboot: Power down` + `Sandbox wiped cleanly.` |

Arranque total: ~4,5 minutos, dominado por `apt` + `npm install -g`. Es exactamente lo que ataca
el paso 2 (golden image).

Extras incluidos: `GELI_KEEP=1` para conservar el qcow2 y el `user-data` al depurar, stderr real
en el fallo de `qemu-img`, y `0700` en `/tmp/sandbox-share-<pid>`.
