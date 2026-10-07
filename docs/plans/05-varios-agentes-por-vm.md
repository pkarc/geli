# Descartado — varios agentes en una misma VM

Decisión complementaria a [04](04-reutilizar-vm.md), pero distinta. Aquella era «reutilizar una VM
que ya está corriendo». Esta es «una VM que lleve dentro más de un agente».

## Qué era, y qué no

No era «implementar capas combinadas». Eso ya está: `layer_key` ordena los nombres y los une con
`+`, y `resolve_chain` recorre los prefijos construyendo una capa por agente. Si algo pidiera
`[claude, agy]` construiría `geli-layer-agy` y encima `geli-layer-agy+claude`, y la sesión vería
los dos binarios.

Lo que faltaba era **una forma de pedirlo** —algo como `geli --with agy claude`— porque
`agent_for_command` mira la primera palabra del comando y devuelve un solo agente. La cadena
siempre tiene un eslabón.

Conviene decir qué **no** es el estado actual, porque es la confusión natural. Las capas son
hermanas, no apiladas:

```
                    ┌─ geli-layer-claude    ← geli claude
alpine ← geli-base ─┼─ geli-layer-opencode  ← geli opencode
                    └─ geli-layer-agy       ← geli agy
```

Ejecutar `claude`, salir y ejecutar `agy` **no suma**. La primera sesión arrancó sobre la capa de
claude y su overlay se borró al salir; la segunda arranca sobre la capa de agy, que cuelga de la
base, y la base no lleva agentes. En esa VM no hay claude. Lo único que se acumula es el caché en
disco: las tres capas quedan guardadas para no reconstruirlas.

## Por qué se descarta

**No se encontró la aplicación.** Es la razón principal y es suficiente. Un agente por VM es lo
que se hace: abres `geli claude` o abres `geli agy`. El caso que justificaría lo contrario —un
agente invocando a otro dentro del mismo sandbox— no es algo que esta herramienta busque hacer, y
quien lo necesite tiene la vía de `geli bash` y montar lo que quiera.

**Y arrastraba una decisión de credenciales sin respuesta buena.** Hoy solo viajan las
credenciales del agente invocado, lo que hace que `geli opencode` no tenga el token de Claude
dentro. Con `--with agy` hay que elegir:

- *no copiar* las de agy → tienes su binario en la VM sin poder usarlo, lo que no sirve de nada;
- *copiarlas* → has ampliado a mano la superficie privilegiada, y el bloque de estado tiene que
  dejarlo clarísimo para que nadie lo haga por costumbre.

Ninguna de las dos es mala por sí sola, pero no hay una obviamente correcta, y pagar esa decisión
por una función sin aplicación clara es el orden equivocado.

## Lo que queda en el código

La maquinaria de apilado sigue ahí y es correcta, pero inalcanzable: nada llamará nunca a
`resolve_chain` con más de un agente. Son unas quince líneas de generalidad no ejercitada —
`layer_key` ordenando y deduplicando, el bucle sobre prefijos, y el test
`layer_key_is_the_set_not_the_order` que comprueba un comportamiento que ningún camino usa.

Es deuda pequeña y vale señalarla: código que *parece* ejercitado y no lo está es una trampa para
quien venga después. Si se decide limpiarla, `resolve_chain` pasa a tomar `Option<&Agent>` y queda
lineal.
