# Hecho — atar cada firma a la conexión que la pidió (2026-10-12)

**Implementado.** El diseño de abajo es el que se construyó, con una cosa que solo apareció al
probarlo contra una VM de verdad y que lo mejora: ver «Lo que cambió al construirlo».


Lo único que `08` dejó abierto: mientras la sesión corre, el guest puede pedir una firma sobre lo
que alcance. Este plan **no** cierra eso. Construye el cimiento sin el cual no se puede cerrar —
saber *con quién* habla la conexión que pide la firma — y deja la decisión de qué hacer con ese
dato (preguntar, o filtrar por la lista `allow` del workspace) para cuando se vea funcionando.

## Por qué no es la bandera que decía el plan anterior

`08` lo dejó escrito como «una bandera que hiciera que geli arrancara su propio agente con
confirmación en vez de usar el del usuario». Eso ya no es lo que hay que hacer. Un agente propio no
sabría nada que el del usuario no sepa: seguiría viendo un desafío sin destino. El sitio donde está
la información es el relay, y hoy la está tirando.

## Lo que el relay tira hoy

`src/ssh_agent.rs:94` contesta `Screen::Unsupported` a toda petición de extensión, y la nota de la
línea 59 explica por qué en voz baja: `ssh` abre **cada** conexión con agente reenviado mandando
`session-bind@openssh.com`, y sigue tan contento cuando le dicen que no.

Esa extensión es justamente la pieza que falta. Su carga, de `PROTOCOL.agent`:

```
byte    SSH_AGENTC_EXTENSION (27)
string  "session-bind@openssh.com"
string  hostkey            ← la clave pública del servidor al otro lado
string  session identifier ← el identificador de esa conexión concreta
string  signature          ← la hostkey firmando ese identificador
bool    is_forwarding      ← si este salto va a reenviarlo a otro
```

Es decir: la petición que estamos rechazando lleva dentro con quién se está hablando, **y una
prueba de ello**. El servidor no puede fabricarla sin su clave privada, y la clave pública se
resuelve a un nombre contra el `known_hosts` del host, que geli ya conoce porque ya lo copia.

Y el lado contrario encaja: lo que firma una autenticación por clave pública no es opaco, es

```
string  session identifier ← el mismo de arriba
byte    SSH2_MSG_USERAUTH_REQUEST
string  usuario
string  servicio ("ssh-connection")
...
```

Así que el relay puede comprobar que la firma que le piden pertenece a la conexión que se vinculó,
en vez de ser una firma suelta para cualquier otra cosa. Eso es lo que hace OpenSSH en su propio
agente cuando la conexión viene vinculada, y es puro: parsear y comparar bytes.

## Qué se construye

Un commit, y deliberadamente **sin política por destino**: nada se bloquea por *hacia dónde* iba.
Una sesión que hoy funciona sigue funcionando igual, y lo que cambia hacia fuera es que la sesión
termina sabiendo decir hacia dónde firmó.

Decir «sin cambio de política» a secas sería falso, y conviene nombrar lo que sí cambia: aparecen
rechazos nuevos. Una conexión que se vinculó y luego pide firmar para otra sesión, o que nombra un
host distinto del que vinculó, ya no pasa. Ninguno de los dos lo produce un cliente honesto —son
justamente los casos que esta pieza existe para ver— pero son comportamiento nuevo, y si algún
cliente real los dispara se verá como un rechazo en mitad de la sesión.

1. **Aceptar `session-bind@openssh.com`.** `Screen` gana un caso, `Bind`, y la extensión deja de
   responderse con un `AGENT_FAILURE` genérico. Cualquier **otra** extensión sigue siendo
   `Unsupported`, en silencio, por la razón que ya está medida.
2. **Verificar la firma de la clave de host.** Sin esto, el guest puede poner el nombre de
   github.com encima de cualquier cosa, porque las claves de host son públicas y reclamar una sale
   gratis. Con esto, solo puede presentar claves que posee — lo que *no* es lo mismo que «claves de
   un servidor real»; ver los huecos al final, donde esa diferencia está escrita entera. Es el
   único coste nuevo de verdad: una dependencia criptográfica en un `Cargo.toml` que hoy tiene tres
   y donde hasta base64 está escrito a mano. Candidata: `ssh-key` de RustCrypto con las
   características de ed25519 y rsa — parsea el formato de clave de OpenSSH y verifica, que son las
   dos cosas que hacen falta. El criterio para aceptarla es que cubra los tipos de clave de host que
   se usan hoy; si solo cubre ed25519, la decisión se replantea antes de escribir el resto.
3. **Estado por conexión.** Cada conexión del relay guarda los bindings que ha visto. `screen()`
   pasa a tomar ese estado, y sigue siendo pura: `(payload, granted, &Bindings) -> Screen`, que es
   lo que mantiene la política testeable sin socket, sin agente y sin VM.
4. **Atar la firma.** Un `SIGN_REQUEST` cuyo blob sea una petición de autenticación y cuyo
   identificador de sesión no coincida con ninguno vinculado se rechaza en voz alta. Una firma que
   **no** es de autenticación —`git commit -S`, `ssh-keygen -Y sign`— no tiene identificador que
   atar, y se reenvía como hoy: es el caso que sigue funcionando bajo `--restrict-net` y romperlo
   sería romper lo único que esa combinación deja útil.
5. **Nombrar el destino.** La clave de host se resuelve contra `known_hosts`. La sesión termina
   diciendo hacia dónde firmó, en la línea final, junto a lo que ya se dice de los ficheros que el
   host va a ejecutar.

## Lo que esto no da, y hay que escribirlo con esas palabras

- **No es política.** Nada se bloquea por el destino todavía. Una sesión puede firmar hacia
  cualquier sitio que alcance, exactamente como ayer; la diferencia es que ahora queda dicho.
- **Un `known_hosts` con los nombres cifrados no se puede resolver al revés.** Si el usuario tiene
  `HashKnownHosts yes` no hay forma de ir de la clave de host al nombre, y lo que se muestra es la
  huella. No es un fallo que arreglar: es el fichero diciendo que no.
- **Un cliente que no sea OpenSSH puede no mandar la extensión.** Entonces no hay binding, y la
  firma se reenvía con el destino sin nombrar. Mientras esto no sea política, da igual; en cuanto
  lo sea, «sin binding» tendrá que ser una decisión explícita y no un hueco.

## Lo que haría falta para darlo por bueno

- Tests puros sobre `screen()` con bindings: una firma atada a su sesión pasa, la misma firma con
  otro identificador no, una firma que no es de autenticación pasa sin binding, y una extensión que
  no es `session-bind` sigue callada.
- Un test de que una firma de host falsificada no vincula nada. Es el test que justifica la
  dependencia; sin él, la dependencia no se gana el sitio.
- Una sesión real: `git push` contra un remoto `ssh://` imprime el destino correcto al terminar, y
  `--restrict-net --ssh` sigue pudiendo `ssh-keygen -Y sign`.
- Y la negativa, que es la que importa: `find / -name 'id_*'` dentro de la sesión sigue sin
  encontrar nada. Esto no toca esa propiedad, y hay que comprobar que no la tocó.

---

## Lo que cambió al construirlo

**El método de autenticación no es `publickey`, y eso resultó ser una noticia buena.** La primera
sesión real autenticó contra GitHub y la línea final dijo `signed 1 not an authentication`: el
parseo obvio busca el método `publickey`, y OpenSSH no se lo pide a un agente desde hace años.
Desde la 9.x pide `publickey-hostbound-v00@openssh.com`, que mete **la clave de host del servidor
dentro de lo que se firma**.

Eso es una fuente de verdad mejor que el binding. El binding lo manda el cliente al principio de la
conexión; el destino hostbound va dentro de la firma, y el servidor al otro lado tiene que aceptar
lo que se firmó, así que el guest no puede elegirlo libremente. El relay usa el de la firma cuando
está, y el del binding si no. Y aparece una comprobación que antes no existía: una firma que nombra
un host y una conexión que vinculó otro no la produce nada honesto, y se rechaza.

Nada de esto se habría visto sin arrancar la VM. El probe contra el agente del host sí mandaba
`session-bind`, y la firma empezaba por el identificador de sesión y el byte 50 — todo lo que el
parseo comprobaba. El método estaba tres campos más adelante.

**Las características por defecto de `ssh-key` no incluyen la criptografía.** `default` es
`ecdsa`, `rand_core` y `std`; sin pedir `crypto` explícitamente, una clave de host ed25519
perfectamente normal se rechaza con «unsupported algorithm», que se lee como una falsificación. El
criterio que el plan ponía a la dependencia —que cubra los tipos de clave de host en uso— se cumple
con esa característica: ed25519, rsa y las tres curvas NIST. DSA queda fuera, y OpenSSH también lo
quitó.

## Lo verificado, contra VMs reales

- `--ssh` con `ssh -T git@github.com`: `Hi pkarc!`, y la línea final dice
  `geli · ssh: signed SHA256:+DiY3wvvV6TuJJhbpZisF/zLDA0zPMSvHdkr4UvCOqU (1)`. Esa huella es la
  ed25519 publicada de github.com, así que el destino que se nombra es el correcto.
- Sale como huella y no como nombre porque las 128 entradas del `known_hosts` de esta máquina están
  cifradas. **El límite que el plan predijo, medido**: no se puede ir de la clave al nombre, y la
  huella es lo que el fichero del usuario es capaz de decir.
- `--restrict-net --ssh`: `ssh-add -l` ve las tres claves, `ssh git@github.com` ni resuelve, y
  `ssh-keygen -Y sign` firma y se cuenta como `1 not an authentication`. La forma que se quería.
- La negativa: `find / -name 'id_*'` dentro de la sesión sigue sin encontrar nada.

## Los huecos que la revisión encontró, y qué se hizo con ellos

Todos salieron de revisar lo construido, no de probarlo:

- **La afirmación de seguridad era más fuerte que el código.** El comentario decía que la clave de
  host verificada es «la de un servidor que participó de verdad en la conexión». No lo es:
  verificar prueba *posesión* de esa clave privada, y el guest puede fabricarse una clave de host y
  firmar bindings con ella. Lo que sí impide es reclamar una clave que no tiene —y las de host son
  públicas, así que sin la comprobación podría haberse puesto el nombre de github.com encima de
  cualquier cosa. Corregido en el código, en `CLAUDE.md` y en `docs/why/`, y fijado con un test
  que lo dice en su nombre.

  **Y la formulación que se escribió entonces seguía siendo demasiado fuerte.** Decía «no puede
  ocultar un destino, solo añadir inventados». La exacta, corregida en `10`: *no puede ocultar una
  firma, pero una firma puede quedarse sin nombre, y eso queda contado*. `publickey` a secas es
  obligatorio en el RFC 4252 y vincular es una extensión que el cliente elige mandar, así que
  ningún servidor puede exigir que la firma llegue nombrada.
- **El `known_hosts` se perdía entero por una línea.** `KnownHosts::read_file` colecciona en un
  solo `Result`: una línea `ssh-dss` vieja —un tipo que ni siquiera habilitamos— y se van las 128
  buenas. Medido con un fichero de dos líneas: error y cero entradas. Y el fallo era silencioso,
  con pinta de «geli no sabe nombrar nada». Ahora se lee línea a línea.
- **Se contaba la firma antes de saber si el agente la concedió.** Un agente bloqueado o que ya no
  tiene la clave responde que no, y la línea final habría dicho «firmó» de algo que no ocurrió. El
  recuento se hace sobre la respuesta, y solo si es una firma. De paso salió de `serve` a una
  función aparte, así que ahora se prueba sin tener un agente que rechace a demanda.
- **Una clave `@revoked` se nombraba como cualquier otra.** Lo más alarmante que esta línea puede
  decir —la sesión firmó hacia un host cuya clave marcaste como revocada— se leía igual que lo más
  rutinario. El marcador va en el texto.
- **Los bindings crecían sin límite.** Son uno por conexión; el guest podía hacer crecer una lista
  dentro del proceso de geli. Tope de 16 y rechazo en voz alta a partir de ahí.

## Lo que sigue abierto

- **Esto todavía no es política.** Nada se bloquea por el destino. Lo que falta para que lo sea es
  la decisión de qué lista manda, y la hay a mano: el `allow` del workspace.
- **Nombrar un host con el `known_hosts` cifrado.** Se puede, pero solo al revés: tomar los nombres
  candidatos de una lista y comprobar su HMAC contra cada entrada. Cuesta sha1 y hmac, y solo sabe
  nombrar lo que ya estaba en una lista — tiene sentido el día que esto sea política, no antes.
- **El canal para preguntar sigue sin existir.** La terminal es del agente mientras la sesión corre.
  Lo que este commit cambia es que ahora *habría algo que decir* en esa pregunta.
- **Solo se mira `~/.ssh/known_hosts`.** Ni el global de `/etc/ssh`, ni un `UserKnownHostsFile`
  puesto en la configuración de ssh. Un host que solo esté en esos sale como huella, que es el
  mismo desenlace que el fichero cifrado y no uno peor.
