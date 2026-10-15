# Plan — los paquetes, el token, y lo que el relay no puede comprobar (2026-10-14)

Cinco frentes abiertos después de `09`. Cuatro salen de una revisión externa; el de los
certificados salió de comprobar esa revisión. El orden no es por gravedad sino por lo que rompe
una garantía ya escrita: la fase 1 contradice una frase de `CLAUDE.md`, y las demás son terreno que
nunca se prometió cubrir.

Cada fase es un commit que deja `cargo test` y `clippy -D warnings` verdes por su cuenta.

---

## Fase 1 — Un nombre de paquete no puede ser una ruta

**Qué está mal.** `valid_package_name` (`src/main.rs:142`) acepta `/`, `.`, `=`, `@` y `+`, y
`build_mount_script` instala los paquetes declarados **antes** del lockdown y como root
(`src/guest.rs:300` contra `:315`).

El agujero no es inyección de shell: los nombres pasan por `shell_quote`, y el comentario de
`build_package_setup` ya explica que esa cita es redundante hoy y por qué se queda. Es la semántica
del gestor:

```json
{"packages": {"npm": ["/workspace/proyecto/evil"]}}
```

`npm install -g` sobre una ruta local ejecuta el script de instalación de ese directorio **como
root y antes de que exista restricción de red**. `pip install ./x` hace lo mismo con `setup.py`. Y
el agente puede escribir `.geli.json`, porque está montado con escritura a propósito, así que se lo
deja preparado a la sesión siguiente.

Eso contradice directamente la regla escrita: *«Nothing in `.geli.json` may grant anything»*.

**Qué se hace.** La validación pasa a ser por gestor, porque los tres no quieren lo mismo:

- **npm** necesita `@ámbito/nombre`, así que la barra se permite exactamente ahí: una sola, después
  de un `@` inicial. Nada que empiece por `.`, `/` o `~`, y nada con `..`.
- **pip** y **apk** no necesitan barra ninguna. Se prohíbe. `apk` conserva `=` para la versión y
  `@` para el repositorio etiquetado; `pip` conserva `==`, `[extras]` no se permite de momento
  porque los corchetes no están hoy y nadie los ha pedido.
- Lo que sobra deja de aceptarse en los tres.

El sitio no cambia: `rejected_package_names` ya para la ejecución antes de construir nada
(`src/main.rs:1450`), con el nombre ofensor en pantalla. El mensaje de error tiene que decir qué
charset aplica a *ese* gestor, no el común de hoy.

**Segundo commit de la fase: la aprobación por hash.** El validador cierra el camino conocido; el
hash cierra el desconocido. En el registro —`~/.config/geli/workspaces/<nombre>.json`, fuera del
alcance de la sandbox, que es la mitad del argumento— se guarda el hash de la lista de paquetes
aprobada. Si `.geli.json` trae otra, la sesión pregunta una vez, por stderr como todo lo
interactivo, y guarda la respuesta. Una lista que no cambia no vuelve a preguntar.

**Tests.** Puros, en `main.rs`: `@scope/pkg` pasa en npm y no en pip ni apk; `./evil`,
`/workspace/x`, `../x` y `~/x` se rechazan en los tres; `pkg=1.2.3` sigue pasando en apk;
`pkg==1.2.3` en pip. Y uno sobre el hash: una lista reordenada es la misma lista y no vuelve a
preguntar — si no, la pregunta sale en cada sesión y se aprende a aceptarla sin leer.

**Lo que esta fase no hace.** No mueve la instalación a después del lockdown. `apk` necesita root,
y tras el lockdown solo hay CONNECT a 443, así que los mirrors de Alpine quedan fuera: es la capa
que más cuesta y la que menos añade una vez que un nombre no puede ser una ruta. Si algún día se
hace, es su propio plan.

---

## Fase 2 — Lo que no se puede comprobar se calla; lo que no cuadra se dice

Dos cosas distintas que hoy acaban en el mismo sitio.

**Lo medido.** Una clave de host firmada por una CA —`ssh-ed25519-cert-v01@openssh.com`—
decodifica en `ssh-key` como `KeyData::Other` y no hay forma de verificarla. Hoy eso cae en
`Refuse("session binding whose host signature does not check out")`: **un rechazo en voz alta, en
mitad de la sesión, contra un servidor perfectamente honesto**. Cualquier empresa que firme las
claves de sus hosts con una CA lo ve. Es exactamente el fallo que `08` existe para evitar: un aviso
que salta cuando no pasa nada tapa el que importa. La autenticación sigue funcionando —sin binding,
la firma se reenvía— así que es falsa alarma y destino sin nombre, no rotura.

**Lo corregido.** La frase de `09` y de `CLAUDE.md` —«el informe no puede ocultar un destino, solo
añadir inventados»— es más fuerte que el código. `publickey` a secas es obligatorio en el RFC 4252
y hostbound es una extensión que cliente y servidor negocian: **ningún servidor puede exigir que la
firma venga nombrada**. Un cliente propio dentro del guest firma sin vincular nada y sale como
«unnamed». La formulación exacta es:

> no puede ocultar una firma, pero puede dejarla sin nombre, y eso queda contado

**Qué se hace.**

1. `Screen` distingue «no sé comprobar este tipo de clave» de «esta firma no cuadra». Lo primero es
   silencioso y deja la conexión sin vincular, igual que un cliente que no manda la extensión. Lo
   segundo sigue siendo ruidoso, porque sigue sin tener explicación honesta.
2. El recuento separa las dos causas de «sin nombre»: el cliente no vinculó, o geli no supo
   comprobar lo que vinculó. Son distintas para quien lee la línea.
3. Se corrige la frase en `CLAUDE.md`, en `docs/why/agents-and-credentials.md` y en `09`.

**La decisión que hay que tomar aquí y no más tarde**: cuando esto sea política, una autenticación
**sin binding se rechaza por defecto**, con una opción explícita para el cliente que no vincula. El
cliente SSH del guest lo elige geli —OpenSSH reciente en Alpine 3.22, que siempre vincula—, así que
el caso honesto no existe en esta imagen. Escribirlo ahora es lo que evita que «sin binding» acabe
siendo un hueco en vez de una decisión.

**Tests.** Un binding con una clave de tipo no soportado da el caso silencioso y no el ruidoso; una
firma mal verificada sigue dando el ruidoso; la línea final distingue las dos clases de «sin
nombre».

---

## Fase 3 — Quién puede hablar con el relay

**Qué está mal, y es más gordo de lo que parecía.** El relay escucha en `127.0.0.1`
(`src/ssh_agent.rs:453`). El socket del agente del host está protegido por el modo del directorio
que lo contiene —`drwx------` en `/run/user/1000/keyring`, comprobado—, o sea solo el uid del
usuario. **Un listener TCP en loopback no tiene permisos: cualquier usuario local de la máquina
puede conectarse.** Mientras la sesión corre, el relay amplía la audiencia de la clave de «mi uid»
a «cualquier uid del equipo».

Eso viene de `08`, no de `09`. Lo que `09` añade es menor y también cierto: una conexión del host
no manda `session-bind`, así que sus firmas pasan por la rama de «sin binding» y se cuentan en el
informe como si fueran del guest.

**Qué se hace.** Un token aleatorio por sesión. El relay lo exige como primeros bytes de cada
conexión y cierra sin contestar si no llega. Frente a vsock o virtio-serial: poco código, conserva
las conexiones simultáneas y no toca la plomería — las mismas razones por las que el proxy CONNECT
ya tiene esta forma.

**El detalle que casi se nos pasa.** El guest llega al relay con `socat` plano
(`build_agent_relay`, `src/guest.rs:239`), y `socat` no puede anteponer bytes a la conexión. El
plan proponía un intermediario de tres líneas:

```sh
#!/bin/sh
# /etc/geli/agent-dial
{ printf '%s' "$GELI_AGENT_TOKEN"; cat; } | socat - TCP:10.0.2.2:PUERTO
```

**Es la parte a medir, no a razonar**: funciona en el papel, y el papel ya se equivocó una vez con
el método de autenticación. Se midió, y no funcionaba — ver abajo.

**Lo que el token defiende y lo que no**, y la nota tiene que decirlo: protege de otros usuarios
del host. No protege del guest, que es quien lee el token en `/etc/geli/env` — y está bien, porque
el guest es el cliente previsto.

**El hermano que queda fuera.** El proxy de egress escucha igual (`src/net.rs:244`) y tiene la
misma exposición. Encaja mejor aún, porque ahí el token cabe en `Proxy-Authorization` sin inventar
nada. Se deja nombrado y fuera de esta fase para no mezclar dos pruebas de VM en un commit.

**Tests.** El relay cierra una conexión sin token y otra con un token equivocado, sin llegar a
hablar con el agente del host; con el token correcto se comporta igual que hoy. Y el de VM real,
que es el que cuenta: una sesión con `--ssh` sigue autenticando.

---

### Lo que cambió al construir la fase 3

El intermediario del shell **no funciona**, y costó ocho arranques de VM averiguar por qué. Con el
`printf` del shell, el token se queda en el búfer del subshell hasta que la conexión se cierra: el
host lo recibe exactamente al final, medido en 4.99s para un cliente que aguantaba 5s. Con un
`printf` externo el token sale en 36µs, pero entonces la **petición** que va detrás se queda
atascada igual, y `ssh-add` se cuelga lo mismo. Los dos fallos se ven idénticos desde dentro, que
es lo que los hace caros de distinguir: un cliente que no responde.

Lo que lo resuelve es dejar de pelear con el shell y hablarle a `socat` un protocolo que ya sabe
hablar: **`PROXY:`**, que escribe un `CONNECT` con `Proxy-Authorization` él solo, en un proceso, y
antes de cualquier dato. El host ya conoce esa conversación porque el proxy de egress es la misma,
y hasta el decodificador de base64 estaba escrito. El guest vuelve a ser una línea de `socat`, sin
fichero intermedio:

```
socat UNIX-LISTEN:/tmp/geli-agent.sock,fork,mode=0600,user=sandbox \
      PROXY:10.0.2.2:agent:1,proxyport=PUERTO,proxyauth=geli:TOKEN
```

Dos detalles que el diseño original no tenía y que salieron de medir:

- **La respuesta se lee hasta la línea en blanco, no por longitud.** Contar los bytes del
  `200 Connection established` se equivocó por uno y se comió el primer frame del agente, que es
  un fallo que parece del protocolo del agente y no lo es.
- **El handshake se lee byte a byte.** Un lector con búfer se tragaría los primeros frames en un
  búfer que esta función descarta al volver.

## Fase 4 — Auditar las dependencias en CI

**Por qué ahora.** `09` pasó de tres dependencias a un `Cargo.lock` 600 líneas más largo, y una de
ellas sostiene una afirmación de seguridad.

`cargo audit` en `check.yml`, que es el gate barato y sin VM, junto a `build`, `test` y `clippy`.

**RUSTSEC-2023-0071 se ignora, con la razón escrita en el fichero de configuración.** Es el ataque
Marvin sobre el crate `rsa`: un canal lateral de tiempo en operaciones con **clave privada**. geli
solo verifica firmas con claves públicas. Y no se puede esquivar quitando la característica: 34 de
las 128 entradas de un `known_hosts` real de esta máquina son `ssh-rsa`, y una clave de host RSA
que no se pueda verificar cae en el rechazo de la fase 2. Mejor ignorarlo hoy con el motivo escrito
que descubrirlo el día que alguien lo vea y se alarme.

---

## Fase 5 — Lo que queda cuando la sesión no termina bien, y por qué la red está abierta

**La limpieza.** `SessionScratch` se encarga del caso normal y `09` no lo tocó, pero un `SIGHUP`
—la terminal se cierra— se lleva el proceso sin pasar por `Drop`, y deja el overlay y
`/tmp/sandbox-share-<pid>/`.

**Contra la propuesta de un manejador de señales, el barrido al arrancar es mejor primero**: cubre
también `SIGKILL`, que ningún manejador puede, y no cuesta dependencia. geli ya sabe hacerlo —
`try_build_lock` guarda el pid dentro del cerrojo justo para decidir si quien lo tomó sigue vivo.
Lo mismo aquí: al arrancar, barrer los restos cuyo pid ya no existe.

El manejador de `SIGHUP` es un segundo paso, y tiene un coste que conviene decir: `libc` está en el
árbol de forma transitiva (0.2.190), pero usarlo obliga a declararlo. Se decide después de ver si
el barrido deja algo sin cubrir.

**La nota de la red.** Por qué el egress está abierto por defecto y `--restrict-net` es opt-in no
está escrito en ningún sitio, y es una decisión, no un descuido: un agente sin red no instala nada
y la mayoría de las sesiones lo necesitan. Va en `docs/why/egress.md`, que es donde alguien lo
buscará.

---

## Lo que este plan no hace

- **No convierte el informe de firmas en política.** Sigue sin bloquearse nada por el destino. La
  fase 2 decide qué pasará con «sin binding» cuando lo sea, que es distinto de hacerlo.
- **No mueve la instalación de paquetes después del lockdown**, por lo dicho en la fase 1.
- **No pone token en el proxy de egress**, aunque tenga el mismo agujero. Es un commit propio.
- **No toca `--ssh-copy-key`.** Sigue siendo la opción débil y sigue avisando.
