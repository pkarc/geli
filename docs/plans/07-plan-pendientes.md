# Plan — cerrar los pendientes de la revisión (2026-10-09)

Plan de implementación para lo que quedó abierto en `06-revision-pendientes.md`, después de
`3c0dc66` (saneo del nombre del workspace y lectura única de `.geli.json`).

Seis fases, cada una un commit que deja `cargo test` y `clippy -D warnings` verdes por su cuenta.
El orden no es por gravedad sino por dependencia: lo barato y aislado primero, para que la fase
grande —sacar la configuración con privilegios del alcance del agente— se escriba sobre un
`main.rs` que ya manda los prompts al sitio correcto.

---

## Fase 1 — Los argumentos conservan sus comillas — **hecha**

**Qué está mal.** `args.agent_args.join(" ")` (`src/main.rs:141`) convierte
`geli claude -p "explica este módulo"` en `claude -p explica este módulo`: el guest vuelve a
partir la cadena por espacios y el agente recibe tres argumentos donde había uno.

**Qué se hace.** `shell_quote` ya existe en `src/guest.rs:189` y hace exactamente lo que hace
falta. Citar cada argumento y unirlos.

El detalle que no se puede pasar por alto: `agent_for_command` mira la primera palabra de la
cadena para decidir qué credenciales viajan. Si la cita primero, la primera palabra pasa a ser
`'claude'` y deja de coincidir con ningún recipe — y el fallo es silencioso en la dirección mala:
la sesión corre sin credenciales en vez de romperse. Así que el emparejamiento se hace sobre
`agent_args[0]` crudo, antes de construir la cadena, y `execute_sandbox` recibe las dos cosas.

- `main`: `let command_to_run = args.agent_args.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ")`.
- `execute_sandbox` toma el nombre del comando aparte, o `agent_for_command` pasa a recibir
  `&[String]`. Lo segundo es menos superficie.

**Tests.** `arguments_keep_their_quoting` sobre la función pura que construye la cadena, con un
argumento con espacios, uno con comilla simple y uno con `$`. Y
`the_agent_is_matched_before_the_command_is_quoted`, que es el que protege el fallo silencioso.

**Riesgo.** Bajo. Único cambio observable: quien dependiera de que `geli claude $FOO` se expandía
en el guest deja de verlo expandido. Es lo correcto, pero va en el mensaje del commit.

---

## Fase 2 — stdout es del comando, también en los prompts — **hecha**

**Qué está mal.** `get_or_create_workspace` y `prompt_enable_persistent_ssh` usan `println!`/
`print!`. Con `geli claude > out.txt` el prompt acaba en el fichero y el usuario mira una terminal
en blanco esperando una pregunta que no ve. Rompe la regla que CLAUDE.md llama «geli's own status
goes to stderr, the command owns stdout».

**Qué se hace.** Pasar a `eprint!`/`eprintln!` todo lo interactivo, incluido el `flush` sobre
stderr. `display_active_workspaces` **se queda en stdout**: ahí el listado no es estado de geli,
es la salida del comando `geli --list`, y debe poder canalizarse.

De paso, y en el mismo commit porque es la misma línea de código: «BINBOX» en
`src/main.rs:564` es un nombre anterior del proyecto.

**Tests.** El prompt lee de stdin y escribe a un descriptor global, así que probarlo pide
inyectar el `Write`. No vale la pena por sí solo; sí vale cuando la fase 5 separe la parte pura.
Esta fase se verifica a mano con `geli --list > /tmp/x` y `geli echo hi > /tmp/y` en un
directorio sin `.geli.json`.

---

## Fase 3 — La configuración con privilegios sale del alcance del agente — **hecha**

La fase que importa. Las otras cinco son higiene.

**Qué está mal.** `.geli.json` vive en el directorio del proyecto, que la sandbox monta **con
escritura**. El host lee ese fichero en el siguiente arranque y le concede lo que pida:

- `"ssh": true` activa `forward_ssh` sin ningún flag (`src/main.rs:151`), y las claves privadas
  del host viajan a la VM. CLAUDE.md dice que reenviar claves es «una decisión de primera mano,
  explícita, por invocación»; hoy es una decisión que puede tomar el propio agente, o un
  repositorio clonado que traiga el fichero.
- `"allow": [...]` amplía el allowlist de la siguiente sesión con `--restrict-net`.
- `"workspace": "otro"` reasigna el directorio y el agente pasa a ver los directorios del otro
  workspace.

El saneo de `3c0dc66` cerró la fuga fuera del registro. No cerró nada de esto.

**La forma final.** Dos ficheros con dos dueños distintos, y eso es toda la regla:

```
proyecto/.geli.json                       declaración del proyecto · no confiable · committeable
  { "packages": { "apk": ["ripgrep"] } }

~/.config/geli/workspaces/github.json     decisiones del usuario · confiable
  { "dirs": ["/home/u/a", "/home/u/b"],
    "ssh": true,
    "allow": ["registry.npmjs.org"] }
```

`.geli.json` queda reducido a `packages`, que ya se trata como no confiable y se valida
(`rejected_package_names`). Eso además lo vuelve committeable sin reparos: «qué herramientas
necesita este proyecto» es un hecho del repositorio y se comparte entre quienes lo clonan, que es
justo lo que `b142544` quería.

**Cómo se averigua el workspace del cwd, ahora que no está en el fichero.** Se busca el cwd entre
los `dirs` de cada workspace del registro. Es un barrido lineal sobre un puñado de ficheros
pequeños que ya se leían enteros para filtrar rutas muertas. Si ninguno lo contiene, es la primera
vez en ese directorio y se pregunta, igual que hoy.

Consecuencia que hay que aceptar y decir: **un repositorio clonado ya no trae su workspace**. Hoy
`.geli.json` lo llevaba dentro, lo cual es precisamente el problema. Clonar y ejecutar `geli`
vuelve a preguntar una vez. Es el precio correcto.

**Formato del registro: de `.txt` a `.json`, sin compatibilidad hacia atrás.** Un fichero por
workspace. El lector entiende `.json` y nada más; los `.txt` que haya se ignoran, y geli lo dice
una vez nombrándolos, porque borrar ficheros del usuario en silencio no es cosa suya.

Deprecar el formato entero en vez de leer los dos es una decisión de etapa: geli no tiene más
usuario que su autor, así que el único registro `.txt` del mundo está en esta máquina. El lector
con doble formato serían treinta líneas de transición escritas para nadie, y el tipo de código que
sobrevive años porque nunca llega el momento evidente de quitarlo.

Y el coste real es casi nulo, porque se solapa con la migración de abajo: cada directorio con un
`.geli.json` lleva dentro el nombre de su workspace, así que la primera vez que entres en él el
registro se reconstruye solo. Lo único que se pierde es la pertenencia de los directorios que no
visites: un workspace de tres carpetas monta solo las que hayas vuelto a pisar, hasta que las
pises todas. En once workspaces de una máquina eso es un rato de uso normal, no una tarea.

**Migración.** En el primer arranque sobre un `.geli.json` con campos viejos:

- `workspace` y `allow` se importan al registro, y se imprime por stderr qué se movió.
- **`ssh` no se importa.** Se avisa de que ya no se lee del proyecto y que hace falta un `--ssh`
  una vez para volver a concederlo. Es el único campo que entrega claves privadas, y
  auto-importarlo desde un fichero que el agente pudo escribir reproduce el agujero que esta
  fase cierra. La migración silenciosa de un `"ssh": true` que el usuario nunca puso lo
  bendeciría como decisión del host.
- Los campos migrados se borran de `.geli.json` y el fichero se reescribe con lo que queda. Si
  queda vacío, se borra.

**Superficie de código.**

- `LocalConfig` pierde `workspace`, `allow` y `ssh`; nace `WorkspaceConfig { dirs, ssh, allow }`.
- `read_local_config` se queda; `get_or_create_workspace` pasa a devolver `WorkspaceConfig` y a
  resolverse contra el registro.
- `get_directories_in_workspace`, `register_directory_to_workspace` y
  `display_active_workspaces` pasan al formato nuevo.
- `save_local_ssh` desaparece: `prompt_enable_persistent_ssh` escribe en el registro.
- `save_local_packages` se queda como está — `packages` no se mueve.
- `migrate_local_config` es una función aparte, y es la que lleva los tests.

**Tests.** Son la parte que justifica la fase:

- `privileged_fields_are_not_read_from_the_project` — un `.geli.json` con `"ssh": true` no
  enciende `forward_ssh`. Es el test de regresión del agujero.
- `migration_moves_workspace_and_allow_but_never_ssh`.
- `migration_leaves_packages_alone`.
- `a_directory_in_no_workspace_is_a_first_run`.

**Riesgo.** El más alto de las seis fases: cambia un formato en disco que ya tiene estado (once
workspaces en esta máquina). Se prueba sobre una copia de `~/.config/geli/` antes de commitear.

---

## Fase 4 — Decir cuándo la sesión dejó algo que el host va a ejecutar

**Qué está mal.** Es inherente a montar el proyecto con escritura, pero hoy no se dice en ninguna
parte. El agente puede escribir `.git/hooks/pre-commit`, `core.hooksPath` o `core.fsmonitor` en
`.git/config`, `.envrc`, `.vscode/tasks.json` o un `Makefile`, y el host los ejecuta la próxima
vez que se usen. El modelo de amenazas que CLAUDE.md declara —«la sandbox existe para mantener al
agente lejos de los ficheros que no se le dieron»— no cubre esta vía, porque estos ficheros **sí**
se le dieron.

**Qué se hace, y qué no.** No se monta `.git/hooks` en solo lectura: rompería `git commit` en
cualquier repositorio con husky o pre-commit instalados, que es la mayoría, y a cambio de poco —el
agente tiene otras cinco vías. Lo que paga es **decirlo**: hash de un conjunto corto de rutas antes
de arrancar la VM y otra vez al salir, y una línea en el bloque final nombrando lo que cambió.

```
geli · la sesión modificó .git/hooks/pre-commit, .envrc
```

El conjunto arranca pequeño y explícito —`.git/hooks/`, `.git/config`, `.envrc`,
`.vscode/tasks.json`— y vive en una constante junto a `NEVER_COPY`, no repartido por
`execute_sandbox`.

**Un detalle de orden.** El hash de salida se toma **antes** de borrar el overlay y la share, y
antes del prompt de `offer_to_persist_packages`, para que el aviso salga aunque el usuario
abandone ese prompt con Ctrl-C.

**Tests.** `changed_paths` es pura sobre dos mapas de ruta→hash: se prueba sin VM. Un test de que
el conjunto vigilado no se solapa con lo que geli misma escribe en el proyecto (`.geli.json`), o
cada sesión que persista un paquete se acusará a sí misma.

---

## Fase 5 — `execute_sandbox` se parte, y la limpieza deja de depender de llegar al final

Dos cambios que se tocan, así que van juntos.

**Qué está mal.** `execute_sandbox` pasa de 400 líneas y mezcla validación, credenciales, proxy,
argumentos de QEMU, bloque de estado y limpieza. Y la limpieza no es a prueba de salidas: hay 12
`process::exit` repartidos por `main.rs`, y un panic o cualquiera de esos deja atrás el overlay
`/tmp/sandbox-session-<pid>.qcow2` y `/tmp/sandbox-share-<pid>/`. CLAUDE.md promete que «la sesión
no deja nada detrás»; hoy lo deja en cuanto algo va mal, que es justo cuando hay ficheros grandes.

**Qué se hace.**

- `SessionPlan`, una struct con todo lo que la sesión necesita —rutas, argumentos de QEMU, el
  documento de cloud-init, el bloque de estado—, construida por una función **pura** a partir de
  `(ws, cur, dirs, cmd, local, policy, proxy_port)`. `execute_sandbox` queda como «construir el
  plan, escribirlo a disco, lanzar, limpiar».
- Un guard con `Drop` dueño del overlay y de la share, que respeta `GELI_KEEP`. Y quitar los
  `process::exit` del tramo que hay entre crear esos ficheros y el final: pasan a `return Err`,
  con `main` traduciendo el error a código de salida.

El `Drop` no corre en un `process::exit` ni en un `abort`, así que quitar los `exit` no es un
extra del cambio: es la mitad que lo hace funcionar.

**Tests.** El plan es puro, así que todo lo que hoy se comprueba arrancando una VM —que cada
directorio se monta, que el allowlist llega al guest, que `--restrict-net` cambia el bloque de
estado— pasa a ser un test de tabla. Y `the_session_leaves_nothing_behind`, que construye el guard
sobre ficheros temporales, lo tira y comprueba que no quedan.

---

## Fase 6 — Los tests se van con su tema

**Qué está mal.** `main.rs` pasa de 2900 líneas y concentra los 86 tests. `agents.rs`,
`guest.rs`, `net.rs`, `qemu.rs` y `ui.rs` no tienen ninguno. Va contra la regla de «módulos por
tema» que el propio CLAUDE.md fija, y hace que un fallo en el proxy se lea en un fichero que no
habla del proxy.

**Qué se hace.** Mover cada test al módulo del que habla: los del proxy y el allowlist a `net.rs`,
los del YAML y los recipes a `guest.rs`, los del cmdline y las imágenes a `qemu.rs`, los de los
recipes TOML a `agents.rs`, el de la línea de carga a `ui.rs`. Lo que quede en `main.rs` es el
registro, la configuración local y el plan de sesión de la fase 5.

Varias funciones tendrán que subir de privadas a `pub(crate)` para ser visibles desde su propio
módulo de tests; donde eso ocurra, es señal de que la función estaba en el fichero equivocado, y
se mueve también.

**Esta fase va la última a propósito.** Es un diff enorme y mecánico, y hacerla antes convertiría
cada una de las cinco anteriores en un conflicto. Después, el diff es un `git mv` conceptual que
nadie tiene que leer línea a línea.

**Tests.** Los mismos 86 y ninguno nuevo. El criterio de que salió bien es que el número no baja
—es fácil perder un test en una mudanza, porque un `#[test]` que no se compila en un módulo que no
se declara no falla: desaparece— así que el commit lleva el conteo antes y después.

---

## Lo que este plan no hace

- **No convierte un destino permitido en algo seguro.** `--restrict-net` con `github.com` abierto
  sigue permitiendo un gist. Es la forma que se eligió, no una carencia.
- **No reenvía el agente SSH en vez de la clave.** Sigue siendo lo que de verdad resolvería la
  fase 3 de raíz —el guest firma y nunca lee la clave— y geli ya tiene la forma: el proxy CONNECT
  es un hilo del host que el guest alcanza en `10.0.2.2:<puerto>`. Falta un relay en el guest que
  presente un socket Unix. Queda para su propio plan.
- **No toca CLAUDE.md como fichero.** La idea de mover la narrativa a `docs/` y dejar la regla con
  un enlace es buena, pero es una decisión sobre cómo se documenta el proyecto, no un pendiente de
  código, y mezclarla con estas seis fases la enterraría.
