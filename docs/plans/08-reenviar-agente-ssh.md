# Hecho — reenviar el agente SSH en vez de la clave (2026-10-11)

Lo único que quedó abierto de la revisión de `06`/`07`. **Implementado**: el diseño de abajo es el
que se construyó, con una corrección que solo apareció al probarlo de verdad (ver el final). Las
reglas resultantes están en `CLAUDE.md`; el porqué, en `docs/why/agents-and-credentials.md`.

## El problema

Con `--ssh`, `collect_ssh_credentials` copia el **fichero** `~/.ssh/id_ed25519` dentro de la VM. El
agente que corre ahí puede leerlo igual que lee cualquier fichero del proyecto. Si esa sesión hace
algo que no debía, la clave salió — y sigue siendo válida mucho después de que la VM desechable se
haya destruido.

La fase 3 cerró que un fichero *dentro* de la sandbox pudiera conceder esa copia. No cerró lo que
pasa cuando tú sí la concedes.

## La forma

Un ssh-agent guarda las claves en memoria y expone un socket Unix. Los clientes no le piden la
clave: le pasan un desafío y le piden que lo firme. **La clave nunca sale del proceso del agente.**
Presentar ese socket dentro del guest, en vez de copiar el fichero:

```
guest: git push
  └─ ssh lee $SSH_AUTH_SOCK = /tmp/geli-agent.sock
       └─ relay en el guest  ──TCP──>  10.0.2.2:<puerto>
                                          └─ hilo en geli (host)
                                               └─ $SSH_AUTH_SOCK del host
                                                    └─ ssh-agent [clave]
```

geli ya tiene la mitad derecha de ese dibujo. El proxy CONNECT de `--restrict-net` es exactamente
esa forma: un hilo dentro de geli escuchando en un puerto efímero del host, que el guest alcanza en
el alias de slirp `10.0.2.2:<puerto>`. Y ya está medido que esa vía aguanta muchas conexiones,
porque es lo que hizo descartar `guestfwd`, que solo aguantaba una (ver `docs/why/egress.md`).

## Lo medido

**1. 9p no puede transportar el socket, así que el relay no es opcional.** Era la suposición de la
que colgaba todo el transporte. El guest *sí* puede crear un socket Unix sobre una carpeta 9p —
`bind()` devuelve OK— y el host *sí* ve el inodo:

```
srwxr-xr-x 1 pkarc pkarc 0 Oct 11 19:18 .geli-xsock
```

Pero conectarse desde el host a ese mismo fichero da `ECONNREFUSED`. 9p transporta el inodo, no la
conexión: el socket es un objeto del kernel del guest y el fichero es solo un nombre. Montar el
socket del host y ya está **no funciona**, en ninguno de los dos sentidos.

**2. `socat` está en los repos de Alpine y pesa 532 KiB instalado.** Es el candidato obvio para el
relay del guest: `socat UNIX-LISTEN:/tmp/geli-agent.sock,fork TCP:10.0.2.2:PORT` es una línea. El
coste es medio mega en la imagen base, que se paga una vez y lo comparten todas las sesiones.

**3. `apk` en una sesión necesita `sudo`.** Un `apk add` directo da `Permission denied` al bloquear
la base de datos. No afecta al diseño, pero sí a cualquier receta que pruebe cosas a mano.

## Decisiones antes de escribir nada

### ¿slirp o vsock?

El dibujo de arriba usa TCP sobre slirp porque es la vía que ya existe. La alternativa es `vsock`
(`-device vhost-vsock-pci`), que da un canal host↔guest nativo, sin red de por medio: más limpio
conceptualmente, y además invisible a la política de egress, que es justo lo que uno quiere para un
canal que no es «salir a internet».

El coste es un requisito nuevo en el host: `/dev/vhost-vsock`, con sus permisos, y `setup.sh`
tendría que proveerlo. Y es exactamente la clase de dispositivo que existe pero no es usable hasta
que una regla de udev te mete en el grupo — el mismo error que ya se cometió con `/dev/kvm` y que
obligó a detectarlo abriéndolo en vez de con `stat`.

**Decidido: slirp.** Medido en esta máquina, un Ubuntu de escritorio corriente:

```
crw-rw----+ 1 root kvm 10, 232  /dev/kvm           <- ACL de udev para el usuario de la sesión
crw-rw----  1 root kvm 10, 241  /dev/vhost-vsock   <- sin ACL
$ id -nG   # pkarc adm cdrom sudo dip video plugdev users lpadmin ollama docker input
```

El usuario **no está en el grupo `kvm`**; KVM le funciona porque udev le pone una ACL a `/dev/kvm`
al iniciar sesión, y `/dev/vhost-vsock` no recibe ese trato. Abrirlo da `EACCES`. QEMU trae
`vhost-vsock-pci` y el módulo del kernel está ahí, así que lo único que falta son permisos — pero
eso significa que una instalación normal necesitaría una regla de udev nueva, añadida por
`setup.sh`, para una función que está apagada por defecto.

Es el mismo tropiezo que `/dev/kvm` ya enseñó: el dispositivo existe y no sirve. La diferencia es
que ahí la respuesta fue detectarlo abriéndolo y degradar a emulación; aquí no hay degradación
posible —o hay canal o no lo hay—, así que el fallo caería en el arranque de cada sesión con
`--ssh`.

La forma de vsock sigue siendo mejor y vale la pena volver a ella si algún día el canal necesita ser
invisible a la política de egress. No para la primera versión.

### ¿Relay tonto o filtro?

El protocolo del agente SSH es simple y está documentado. Un relay tonto copia bytes; uno que
entienda el protocolo puede **rechazar mensajes**:

- `SSH_AGENTC_ADD_IDENTITY` / `REMOVE_IDENTITY` — el guest no tiene por qué modificar tu agente.
  Hoy, con el agente reenviado entero, puede borrarte las claves cargadas.
- `REQUEST_IDENTITIES` — el guest no tiene por qué enumerar *todas* tus claves, solo usar la que le
  concediste. Esto es lo que hace que `--ssh-key` siga significando algo.

Permitir solo `SIGN_REQUEST` para una clave concreta, y nada más, es poco código y cambia bastante
la superficie. **Inclinación**: filtro desde el principio. Un relay tonto es más fácil de escribir y
más difícil de justificar.

### ¿Qué pasa con `--restrict-net`?

Hay un conflicto de forma. El ruleset de `src/guest/egress.nft` es default-drop con **una** regla,
para **un** puerto, y esa simplicidad es deliberada. El agente pide un segundo puerto permitido.

Pero la exposición ahí es mucho menor, y merece decirse: bajo `--restrict-net` el guest no alcanza
ningún servidor SSH, así que un agente reenviado no le sirve para autenticarse contra nada. Solo
sirve para lo local — firmar commits— que es precisamente lo que hoy justifica reenviar claves bajo
esa bandera.

**Inclinación**: una segunda regla, explícita, con el puerto del agente. Y no multiplexar sobre el
proxy: un proxy que además habla el protocolo del agente deja de ser una cosa que se pueda explicar
en una frase.

### ¿Y si el usuario no tiene agente?

Esto solo sirve si ya corres un `ssh-agent` con claves cargadas. Si no hay `$SSH_AUTH_SOCK`, no hay
nada que reenviar. Las opciones son caer a copiar el fichero —silenciosamente, que es lo peor— o
fallar diciéndolo. **Inclinación**: fallar diciéndolo, con la frase que explica qué hacer
(`ssh-add`). Caer al modo menos seguro sin avisar es exactamente la clase de fallo que el resto de
este proyecto se dedica a evitar.

## Lo que esto **no** arregla

Y es la razón por la que esto no se da por hecho.

**Mientras la sesión corre, el guest puede pedir firmas de lo que quiera.** No puede robar la clave,
pero puede autenticarse como tú contra cualquier host que alcance, durante toda la sesión. Es la
debilidad clásica del reenvío de agente, y la recomendación de siempre es no reenviarlo a máquinas
en las que no confías — que es literalmente el caso de uso de geli.

Así que el cambio **no** es «ahora es seguro». Es:

> «la clave sigue siendo tuya para siempre» en vez de «la clave puede acabar siendo de otro».

El daño pasa de permanente a acotado a la duración de la sesión. Es una mejora real y no una cura, y
la documentación tiene que decirlo con esas palabras, igual que hoy dice que un destino permitido
sigue siendo una vía de salida.

**La mitigación barata es `ssh-add -c`**, que hace que el agente pregunte en cada firma. Convierte
«puede actuar como tú en silencio» en «puede actuar como tú si le das al intro». Rompe cualquier uso
no interactivo, así que debería ser una bandera aparte y no el comportamiento por defecto — pero el
aviso que hoy sale con `--ssh` debería nombrarla.

## Fases

1. ~~**Medir vsock.**~~ Hecho: descartado por permisos, ver arriba. El transporte es slirp.
2. **El lado del host.** Un hilo como el del proxy: acepta conexiones, habla el protocolo del
   agente, deja pasar solo `SIGN_REQUEST` para la clave concedida, reenvía al `$SSH_AUTH_SOCK` del
   host. Es la parte con lógica, es pura salvo el socket, y lleva los tests.
3. **El lado del guest.** `socat` en la imagen base, el relay arrancado desde `mounts.sh`, y
   `SSH_AUTH_SOCK` exportado en `/etc/geli/env` como el resto de las variables de sesión.
4. **La bandera.** `--ssh` pasa a significar «reenvía el agente». Copiar el fichero deja de ser el
   camino por defecto, y si se conserva es bajo otro nombre que diga lo que hace.
5. **`--restrict-net`.** Segunda regla en `egress.nft`, y el aviso reescrito: hoy dice que SSH no
   llega a ningún sitio, y seguirá siendo verdad, pero la frase sobre «las claves siguen sirviendo
   para firmar commits» pasa a referirse al agente.

## Lo que haría falta para darlo por bueno

- Un test de que el relay rechaza todo lo que no sea `SIGN_REQUEST`. Es la pieza que convierte esto
  en algo mejor que reenviar el agente a pelo.
- Una sesión real que haga `git push` contra un remoto `ssh://` sin que exista ningún fichero de
  clave dentro de la VM. Eso es la prueba, no que el socket aparezca.
- Y la comprobación negativa, que es la que de verdad importa: `find / -name 'id_*'` dentro de la
  sesión no encuentra nada.

---

## Lo que cambió al construirlo

Casi nada del diseño, y una cosa que ninguna prueba unitaria habría encontrado.

**Rechazar y avisar no son lo mismo.** La primera sesión real imprimía un aviso de rechazo en
*cada* conexión ssh normal. No era un fallo del filtro: `ssh` abre toda conexión a un agente
reenviado con una petición de extensión (`session-bind@openssh.com`) y sigue tan contento cuando el
agente dice que no. Pero un aviso que salta cuando no pasa nada es un aviso que la gente aprende a
saltarse — y el que taparía es el del guest intentando reescribir tu agente. Así que `Screen` tiene
ahora tres casos y no dos: reenviar, rechazar en voz alta, y rechazar en silencio.

## Lo verificado, contra VMs reales

- `--ssh`: `ssh -T git@github.com` responde `Hi pkarc! You've successfully authenticated` y
  `find / -name 'id_*'` dentro de la sesión no encuentra nada. Esa es la prueba, no que el socket
  aparezca.
- El filtro: `ssh-add -D` y `ssh-add <clave nueva>` desde dentro fallan, y el agente del host sigue
  con sus tres claves.
- `--ssh-key`: el guest ve **una** identidad en vez de tres.
- `--restrict-net --ssh`: `ssh-add -l` funciona, `ssh git@github.com` ni siquiera resuelve, y
  `ssh-keygen -Y sign` llega a firmar. Que es exactamente la forma que se quería.
- Sin agente: falla con la orden que lo arregla, y no cae al modo débil.
- `--ssh-copy-key`: sigue funcionando y lo dice en el bloque de estado con palabras que no se
  pueden confundir.

## Lo que sigue abierto

Nada de esto cambia el límite de fondo, y la documentación lo dice con esas palabras: mientras la
sesión corre, el guest puede pedir una firma sobre lo que alcance. `ssh-add -c` es la mitigación y
la nombra el aviso, pero no está integrada — sería una bandera que hiciera que geli arrancara su
propio agente con confirmación en vez de usar el del usuario, y eso es otro diseño.
