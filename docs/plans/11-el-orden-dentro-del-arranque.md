# Plan — el orden dentro del arranque, y los dos puertos que quedan (2026-10-15)

Cuatro fases que salen de revisar `10` con alguien de fuera. Ninguna arregla un fallo: arreglan
*cuándo* pasan cosas que ya pasaban, que es donde queda el riesgo cuando lo demás está cerrado.

El hilo común es el mismo que en `09` y `10`: lo que una cosa alcanza depende del momento en que
ocurre, y ese momento estaba elegido por comodidad y no por lo que cuesta.

---

## Fase 1 — Que un `postinstall` no tenga nada que llevarse

**Qué está mal.** Los paquetes declarados se instalan como root, con red, **después** de montar el
workspace y **después** de que `write_files` haya dejado las credenciales del agente en
`/home/sandbox`. La aprobación de `10` cubre nombres, no versiones: un paquete aprobado hace un
año que se compromete mañana trae un `postinstall` que corre con el proyecto montado, los tokens
del usuario en disco y salida a internet.

**Qué se hace, que es solo reordenar.** El guion de montajes pasa a:

```
cachés (npm, pip, apk)  →  paquetes  →  workspace  →  credenciales  →  relay  →  lockdown
```

- **El workspace se monta después de instalar.** Antes no se podía, porque un nombre de paquete
  podía ser una ruta del proyecto; el validador de `10` lo impide, así que ya no hay dependencia.
  Nada más entre medias la necesita: los nombres de carpeta se calculan en el host, el `cd` vive
  en el perfil de login y los cachés se montan aparte.
- **Las credenciales dejan de ir en `write_files` directo al home.** Se escriben en
  `/etc/geli/secrets/`, root y `0600`, y se instalan en `/home/sandbox` después de los paquetes.
  Es exactamente lo que la receta base ya hace con sus ficheros, por la misma razón: `write_files`
  corre antes que `runcmd`, así que lo que se escriba ahí existe antes que nada de lo que decidimos.
- **`cd /` explícito antes de instalar**, y el validador rechaza `.tgz`, `.tar`, `.tar.gz`, `.whl`
  y `.zip`. npm y pip leen esos nombres como ficheros locales; hoy es inofensivo porque el
  directorio de trabajo de `runcmd` es `/`, donde el agente no escribe — pero eso es una
  protección que depende del directorio de trabajo y no del validador, y es gratis no depender.

**Lo que esta fase no cierra por sí sola.** El instalador sigue siendo root, así que puede leer
`/etc/geli/secrets` aunque sea `0600`. Con la fase 2 deja de poder. **Son un par**: sin el
reordenado, quitar root no impide esperar al montaje; sin quitar root, el reordenado no impide
leer los secretos. Se parten en dos commits porque se prueban distinto, no porque sirvan solos.

**Tests.** El orden, que es lo único que esta fase cambia: paquetes antes que los montajes del
workspace, credenciales después de los paquetes, lockdown el último y el marcador de listo detrás
de todo. Más el validador con las cinco terminaciones.

---

## Fase 2 — npm y pip sin root

**Qué está mal.** `apk add`, `npm install -g` y `pip install` corren como root antes del lockdown.
Para apk es inevitable y es defendible: sus paquetes van firmados por Alpine, y el validador ya
impide añadir repositorios o pasar `--allow-untrusted`. Para npm y pip no lo es: ahí el contenido
es de quien lo publicó.

**Qué se hace.** npm y pip pasan a correr como `sandbox`. Dos cosas medidas en el guest que
cambian el diseño respecto a lo que se propuso:

- **`runuser` no existe en Alpine.** Hay `su` (busybox) y `doas`. Se usa uno de los dos, y cuál
  se decide probando que `HOME` llega bien, porque de eso depende dónde busca pip su caché.
- **`npm config get prefix` es `/usr/local` y el `PATH` del sandbox no incluye nada de su home.**
  Así que esto no es solo cambiar quién instala: hay que instalar en `/home/sandbox/.local` y
  exportar el `PATH` en `/etc/geli/env`, que es de sesión y no obliga a reconstruir la imagen.

**Lo que compra, dicho con precisión: la integridad del lockdown, no los secretos.** Un
`postinstall` sin root no puede tocar `nft` ni los sudoers antes de que se cierren. Que no pueda
leer las credenciales es cosa de la fase 1, y solo funciona con las dos puestas.

**Tests.** Que el guion instale npm y pip bajo el usuario y apk no, que el `PATH` exportado nombre
el prefijo, y una sesión real donde un paquete declarado acabe ejecutable para el agente — que es
lo único que prueba que el `PATH` y el prefijo coinciden.

---

## Fase 3 — El otro puerto sin autenticar

**Qué está mal.** El proxy CONNECT escucha en `127.0.0.1` sin pedir nada (`src/net.rs:244`), que
es la misma exposición que el relay tenía hasta `10`: un puerto TCP no tiene permisos, así que
cualquier usuario local del host lo puede usar como proxy abierto hacia los destinos permitidos, y
sus conexiones ensucian el `net: N allowed, M blocked` del final igual que ensuciaban el informe
de firmas.

**Qué se hace.** El mismo token de sesión, y aquí es más fácil que en el relay: no hace falta
inventar nada en el guest, porque la URL del proxy admite credenciales y ya se exporta en seis
variables (`build_proxy_env`) — `http://geli:TOKEN@10.0.2.2:<puerto>`.

**Un detalle de colocación**: `authorised()` vive hoy en `ssh_agent.rs` y es código HTTP. Se mueve
a `net.rs`, que es su sitio, y el relay lo importa desde ahí. Al revés sería la dependencia
equivocada.

**Tests.** Una petición sin credenciales no pasa y no cuenta; con ellas, todo igual que hoy. Y una
sesión real con `--restrict-net`, que es donde el proxy es el único camino.

---

## Fase 4 — Una raíz de staging por usuario

**Qué está mal.** `staging_dir` compone `/tmp/<nombre>-<pid>`, hace `create_dir_all` y luego
`chmod 0700`. El sticky bit de `/tmp` impide borrar lo ajeno, **no crear un nombre**: otro usuario
puede plantar ese directorio —o un enlace simbólico con ese nombre— antes que nosotros. El
`chmod` fallaría y la sesión moriría, que es lo benigno; lo que no es benigno es escribir el
`user-data` a través de un enlace que apunta a otro sitio.

**Qué se hace.** Una raíz por usuario, creada y **verificada** una vez — dueño y modo — en vez de
una ruta nueva y adivinable por sesión. `$XDG_RUNTIME_DIR/geli` para lo pequeño.

**Y no para el overlay.** Medido en esta máquina: `/tmp` es tmpfs de 16G y `/run/user/1000` tiene
3.2G. El qcow2 de sesión puede crecer, así que mandarlo ahí cambia un problema de seguridad por
uno de espacio. Va a `/tmp/geli-<uid>`, con la misma comprobación de dueño y modo.

**Y el barrido mira el dueño.** Hoy lo protege que borrar lo ajeno falle. Con `hidepid` montado en
`/proc`, los pids de otros usuarios parecen muertos, y conviene no depender de que el borrado
falle para no borrar lo que no es nuestro.

**Tests.** Que una raíz con el modo o el dueño equivocados se rechace en vez de usarse, y que el
barrido deje en paz lo que no es del usuario.

---

## Lo que este plan no hace

- **No mueve la instalación de paquetes después del lockdown.** Sigue sin poder: apk necesita root
  y el proxy solo marca 443. Lo que cambia es que ya no hace falta para lo que importaba.
- **No convierte el informe de firmas en política**, ni escribe el test de «sin binding se
  rechaza». Ese test se escribe el día que exista la política, que es cuando puede fallar.
- **No añade un manejador de `SIGHUP`.** El barrido de `10` cubrió el caso medido, y un manejador
  no cubriría `SIGKILL` de todas formas.
