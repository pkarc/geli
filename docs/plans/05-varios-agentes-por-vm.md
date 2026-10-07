# Descartado — varios agentes en una misma VM

Decisión complementaria a [04](04-reutilizar-vm.md), pero distinta. Aquella era «reutilizar una VM
que ya está corriendo». Esta es «una VM que lleve dentro más de un agente».

## Qué era, y qué no

No era «implementar capas combinadas». Eso ya estaba: `layer_key` ordenaba los nombres y los unía
con `+`, y `resolve_chain` recorría los prefijos construyendo una capa por agente. Si algo hubiera
pedido `[claude, agy]` habría construido `geli-layer-agy` y encima `geli-layer-agy+claude`, y la
sesión vería los dos binarios. Nada lo pidió nunca, y al tomar esta decisión esa maquinaria se
quitó — ver más abajo.

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

## El código, ya limpiado

La maquinaria de apilado se quitó en el mismo momento de tomar la decisión, en vez de dejarla como
generalidad inalcanzable. Código que *parece* ejercitado y no lo está es una trampa para quien
venga después.

| antes | ahora |
|---|---|
| `resolve_chain(dir, &[&Agent])` recorriendo prefijos | `resolve_image(dir, Option<&Agent>)`, lineal |
| `layer_key()` ordenando, deduplicando y uniendo con `+` | eliminada; el nombre de la capa es el comando del agente |
| `ensure_layer(..., key, ...)` | sin `key`: lo deriva de `agent.command`, así que no hay dos fuentes que puedan discrepar |
| `build_layer_cloud_init(agent, uid, key)` | `build_layer_cloud_init(agent, uid)` |
| test `layer_key_is_the_set_not_the_order` | eliminado: comprobaba un comportamiento que ningún camino usaba |

Probado con la disciplina habitual del repo: `dump_generated_documents` antes y después del
refactor, **13 documentos del invitado byte a byte idénticos**. Y las tres rutas verificadas en
vivo — un agente sobre su capa, un comando que no es agente sobre la base sin agentes, y una capa
ausente que se construye sola en el primer uso.
