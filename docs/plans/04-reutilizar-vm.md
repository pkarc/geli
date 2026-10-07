# Descartado — reutilizar una VM en vez de arrancar otra

## La pregunta

Si corro tres agentes en tres pestañas de tmux, ¿están en la misma VM? ¿Y podría geli, si ya hay
una levantada, usarla en vez de arrancar otra completamente independiente?

A la primera: **no.** Cada invocación lanza su propio QEMU con su propio overlay. Medido con tres
sesiones de `claude` simultáneas:

```
PID       RSS
1275692   373 MB
1275693   351 MB
1275694   343 MB
          ------
          1071 MB en 3 VMs
```

Unos 350 MB por pestaña, no los 2 GB del techo — el techo no es una reserva.

## Qué se ganaría

Arranque instantáneo en lugar de ~10 s, y una VM en lugar de N.

## Por qué se descarta

Cuatro razones, y las tres primeras son propiedades que hoy salen gratis *por haber una VM por
sesión*. Reutilizar no es optimizar el diseño: es cambiarlo.

**1. Las credenciales dejan de estar aisladas.** Hoy `geli opencode` no tiene el token de Claude
en su VM, porque solo viajan las credenciales del agente invocado. En una VM compartida, el primer
agente que entra deja la suya dentro y el segundo la lee. Esto es exactamente lo contrario del
requisito que motivó el campo `credentials` por agente.

**2. La política de red no admite dos valores.** Una pestaña con `--restrict-net` y otra sin: el
cierre es de máquina entera — ruta borrada, nftables, sudoers recortado. Ganaría quien llegara
primero, y el bloque de estado de la otra sesión mentiría. La regla ya establecida en este
proyecto es que *una restricción que el sandbox puede deshacer es peor que ninguna, porque la
línea de estado afirma que está puesta*. Una que depende de quién arrancó antes es ese mismo
problema.

**3. Los montajes difieren.** Pestaña A en el workspace `foo`, B en `bar`: la VM compartida
tendría que montar los dos, y el agente de B vería los ficheros de `foo`. Eso rompe la frontera de
ficheros, que es el objetivo original del sandbox.

**4. Se pierde el ser desechable.** Hoy una sesión no deja nada: overlay y directorio temporal
borrados al salir. Una VM compartida acumula estado entre sesiones — `/tmp`, paquetes instalados,
lo que hiciera el agente anterior.

Y el transporte tampoco es gratis: la consola serie tiene un solo consumidor, así que una segunda
sesión interactiva exigiría o `sshd` en la imagen —que la receta base **quita** a propósito, junto
con chronyd— o un agente en el invitado por vsock.

## El término medio, y por qué tampoco

Se podría reutilizar solo cuando coincida la firma completa de la sesión: mismo workspace, mismo
agente, mismo modo de red. Ahí las tres primeras objeciones se caen — las credenciales ya
estarían, los montajes son los mismos, la política es la misma.

Pero entonces la pregunta es cuándo ocurre eso. ¿Cuándo corres el mismo agente, en el mismo
workspace, con la misma política, en dos pestañas? Casi nunca, y si lo haces probablemente quieres
que no se pisen. **Los casos en que compartir es seguro son los casos en que menos lo quieres**, y
lo que se compra son 10 segundos y 350 MB.

## Qué se hizo en su lugar

El arranque ya se resolvió por otra vía: imagen base, capas por agente y arranque directo del
kernel llevaron la sesión de 4,5 min a ~9 s. Eso era el 90% del motivo para querer reutilizar.

Lo que sí hacía falta era que **las sesiones concurrentes no se estorbasen**, y ahí había un fallo
real. Las rutas por sesión van por PID y el proxy escucha en un puerto efímero, así que tres
sesiones a la vez funcionan. Pero las capas se construyen *en el primer uso*, y dos sesiones que
estrenaban el mismo agente competían por los mismos ficheros. Medido antes del arreglo, dos
`geli agy` con un segundo de diferencia:

```
A:0   → 1.3.1
B:1   → (vacío)    Error: Os { code: 2, kind: NotFound }
```

La segunda borraba el overlay a medio construir de la primera y moría con un error crudo. Ahora
hay un cerrojo `O_EXCL` por imagen: una construye, las demás esperan y encuentran la capa hecha.

```
A:0   → 1.3.1
B:0   → 1.3.1      "[*] Another geli is building the agy layer. Waiting for it."
```

El cerrojo lleva dentro el pid de quien lo tiene, porque una construcción fallida sale por
`process::exit` y se salta el `Drop` del guardia. Verificado plantando un cerrojo con un pid
muerto:

```
[*] Another geli is building the agy layer. Waiting for it.
[*] That build died without finishing. Taking over.
[✓] agy layer ready.
```

## Pendiente de esto

Las cachés de npm y pip son un 9p compartido entre invitados concurrentes, y npm no es amable con
accesos simultáneos a su caché. No se ha observado ningún problema, pero tampoco se ha probado con
dos sesiones instalando paquetes a la vez: está sin medir, no descartado.
