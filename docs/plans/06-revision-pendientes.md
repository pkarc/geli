# Pendiente — lo que encontró una revisión del proyecto (2026-10-09)

> El plan de implementación de lo que queda abierto está en `07-plan-pendientes.md`.

Una lectura de conjunto de la rama `persistent-history-disk`, en `b413ce1`. Ordenado por gravedad.
Cada punto dice dónde está y qué haría; los que ya se hicieron lo dicen.

## 1. El agente puede escribir el `.geli.json` que configura su próxima sesión

`.geli.json` vive en el directorio del proyecto, y ese directorio se monta con escritura en la VM.
Lo que el agente escriba ahí lo lee el host en la siguiente sesión, sin preguntar:

- **`"ssh": true`** — `read_local_ssh` (`src/main.rs`) activa `forward_ssh` y las claves privadas
  del host viajan a la VM. CLAUDE.md dice que reenviar claves es «una decisión explícita, por
  invocación»; esto la convierte en algo que el propio agente puede decidir. Lo mismo vale para un
  repositorio clonado que traiga ese fichero.
- **`"allow": [...]`** — el agente amplía su propia lista de dominios para la próxima sesión con
  `--restrict-net`.
- **`"workspace": "otro"`** — el directorio se une a otro workspace, y el agente pasa a ver los
  directorios de ese otro.

Qué haría: separar la configuración en dos.

- Lo que da privilegios (`ssh`, `allow`, el vínculo con el workspace) pasa al host, en
  `~/.config/geli/`, fuera del alcance del agente. **Pendiente.**
- En el proyecto queda solo lo inofensivo, como `packages`, que ya se trata como no confiable y se
  valida.

**Hecho — el path traversal.** El nombre del workspace tampoco se saneaba al leerlo del fichero:
solo se saneaba el que se teclea en el prompt, y el del fichero terminaba en
`config_dir.join(format!("{}.txt", ws))` —admitía `../`— y como hostname en el `meta-data`.
`sanitize_workspace_name` es ahora la única regla y corre en ambos caminos; un nombre que no
sobrevive es un error y uno que hubo que cambiar se dice por stderr. Esto cierra la fuga fuera del
registro, no la escalada de privilegios: `"ssh": true` sigue siendo cosa del punto de arriba.

## 2. Lo que el agente deja en el repositorio se ejecuta en el host

Es inherente a montar el proyecto, pero hoy no se dice. El agente puede escribir
`.git/hooks/pre-commit`, `core.hooksPath` o `core.fsmonitor` en `.git/config`, `.envrc`,
`.vscode/tasks.json` o un `Makefile`, y el host los ejecuta la próxima vez que se usen. El modelo de
amenazas («el agente no toca lo que no se le dio») no cubre esta vía.

Qué haría:

- Montar `.git/hooks` en solo lectura, o tomar un hash antes y después de la sesión.
- Avisar en la línea final si cambiaron `.git/hooks`, `.git/config` o `.envrc`
  (`geli · .git/hooks modificado`). Coincide con la regla de que la línea de estado no miente ni
  por omisión.

## 3. Los argumentos pierden sus comillas

`agent_args.join(" ")` convierte `geli claude -p "explica este módulo"` en
`claude -p explica este módulo`. CLAUDE.md lo menciona como algo que hay que saber; mejor
arreglarlo. Citar cada argumento con `shlex`/`shell-words` antes de unirlos son pocas líneas, y es
una función pura, así que se prueba sin VM.

## 4. Estructura

- **`main.rs` pasa de 2900 líneas, y la mitad son tests.** Todos viven ahí; los demás módulos no
  tienen ninguno. Va contra la regla de «módulos por tema»: los del proxy van en `net.rs`, los del
  YAML en `guest.rs`, los del cmdline en `qemu.rs`.
- **`execute_sandbox` pasa de 400 líneas** y mezcla validación, credenciales, proxy, argumentos de
  QEMU, bloque de estado y limpieza. Separar «preparar la sesión» (pura, testeable) de «lanzarla».
- **Hecho — la lectura de `.geli.json`.** Se leía y parseaba cuatro veces
  (`read_local_allowlist`, `read_local_packages`, `read_local_ssh`, `get_or_create_workspace`).
  Ahora hay un `read_local_config` que devuelve un `LocalConfig`, y `main` lo pasa a
  `execute_sandbox`. Eso resuelve además la incoherencia: un JSON mal formado se ignoraba en
  silencio en tres sitios y era un error en el cuarto, y los dos escritores lo sobrescribían con
  el default llevándose el resto del fichero por delante. Ahora es un error en todas.
- **La limpieza no es a prueba de salidas.** Un `process::exit` o un panic a mitad deja el overlay
  y `/tmp/sandbox-share-<pid>` atrás. Un guard con `Drop`, y sin `exit` en medio.
- **Los prompts interactivos escriben en stdout** (`println!` en `get_or_create_workspace` y
  `prompt_enable_persistent_ssh`). Rompe la regla de que stdout es del comando: con
  `geli claude > out.txt` el prompt acaba en el fichero y el usuario no lo ve.

## 5. Menores

- CLAUDE.md es excelente como historia, pero se carga entero en cada sesión. La narrativa de cómo
  se llegó a cada decisión puede vivir en `docs/` y dejar en CLAUDE.md la regla y un enlace.
- `display_active_workspaces` imprime «BINBOX», un nombre antiguo.
