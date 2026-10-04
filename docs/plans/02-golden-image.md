# Paso 2 — Golden image

## Contexto

El paso 1 dejó el sandbox funcionando, pero cada invocación tarda **~4,5 minutos**, y casi todo
ese tiempo es `apt-get install nodejs npm python3` más `npm install -g @anthropic-ai/claude-code`,
repetidos en cada arranque. Es el factor que decide si la herramienta se usa a diario o se
abandona.

`setup.sh` ya descarga la imagen base de Ubuntu en `~/qemu-sandbox/`. La idea es aprovechar ese
punto: provisionar **una vez** sobre esa imagen y que cada sesión sea solo un overlay encima,
sin instalar nada.

**Resultado esperado:** el arranque de sesión baja de ~4,5 min a ~30 s, y el contenido del
invitado (incluido `git`, que hoy falta) queda horneado.

Decisiones ya acordadas: provisión como subcomando en Rust, contenido = lo actual + `git`, y si
la imagen falta → error con instrucción (si está desactualizada → aviso pero arranca igual).

## Enfoque

### Disposición de imágenes en `~/qemu-sandbox/`

| Fichero | Rol |
|---|---|
| `ubuntu-24.04-server-cloudimg-amd64.img` | Base prístina que descarga `setup.sh`. Nunca se escribe. |
| `geli-golden.qcow2` | Overlay sobre la base, ya provisionado, `SANDBOX_DISK_SIZE`. Lo crea `geli --build-image`. |
| `geli-golden.recipe` | Hash de la receta con la que se construyó, para detectar imágenes viejas. |

La cadena queda base ← golden ← sesión. La base se mantiene intacta, así que reconstruir la
golden es barato y no requiere volver a descargar 600 MB.

### Refactor: extraer lo que ambos caminos comparten

`execute_sandbox` (`src/main.rs`) hace hoy: preflight → cloud-init → ISO → overlay → args de
QEMU → spawn → limpieza. La construcción de la golden necesita casi lo mismo con otro cloud-init
y sin los shares 9p del proyecto. Extraer, sin cambiar comportamiento:

- `fn preflight() -> io::Result<PathBuf>` — verifica `qemu-system-x86_64`/`genisoimage` y
  devuelve la ruta de la imagen base (hoy inline).
- `fn make_cloud_init_iso(dir: &Path, user_data: &str, instance_id: &str) -> io::Result<PathBuf>`
- `fn create_overlay(backing: &Path, target: &str, size: Option<&str>) -> io::Result<()>`
- `fn run_qemu(disk: &str, iso: &Path, extra_args: Vec<String>) -> io::Result<()>`

`execute_sandbox` y el nuevo `build_golden_image` se componen de estas.

### Qué se hornea en la golden

Nueva función pura `build_golden_cloud_init() -> String`, al lado de `build_cloud_init`:

```yaml
#cloud-config
users: [default, sandbox (NOPASSWD, lock_passwd)]
package_update: true
packages: [nodejs, npm, python3, python3-pip, git]

write_files:
  - /etc/systemd/system/serial-getty@ttyS0.service.d/autologin.conf
  - /home/sandbox/.bash_profile      # ver abajo
runcmd:
  - npm install -g @anthropic-ai/claude-code
  - apt-get clean && rm -rf /var/lib/apt/lists/*
  - cloud-init clean --logs --seed
  - poweroff
```

El autologin y el `.bash_profile` pasan a la golden porque son **estáticos**: no dependen del
workspace. Eso vacía casi entero el cloud-init de sesión.

**El `.bash_profile` horneado resuelve una carrera.** Si el autologin viene en la imagen, el getty
puede dar sesión *antes* de que cloud-init haya escrito el comando de esta sesión. Por eso espera:

```bash
[ -f ~/.bashrc ] && . ~/.bashrc
cloud-init status --wait >/dev/null 2>&1
[ -f /etc/geli/env ] && . /etc/geli/env
if [ -f /etc/geli/session ]; then
  . /etc/geli/session
  sudo poweroff
else
  echo "[!] geli: no session script found; cloud-init may have failed."
  echo "[!] See /var/log/cloud-init-output.log. Dropping to a shell."
fi
```

El `else` es deliberado: si cloud-init falla, en vez de apagar a ciegas deja una shell desde la
que leer el log. Es el fallo silencioso del paso 1, pero esta vez con red de seguridad.

### Qué queda en el cloud-init de sesión

`build_cloud_init` se reduce a escribir `/etc/geli/env`, `/etc/geli/mounts.sh` y
`/etc/geli/session` (el `cd` más el comando del usuario), y un `runcmd` con los montajes, los
`chown` y nada más. Desaparecen `apt`, `npm`, el `install` del `.bash_profile` y el reinicio del
getty.

### Detección de imagen obsoleta

`GOLDEN_RECIPE_HASH`: hash del texto de `build_golden_cloud_init()` calculado en tiempo de
ejecución con `std::hash::DefaultHasher`, no una constante que haya que acordarse de subir.
`--build-image` lo escribe en `geli-golden.recipe`; al arrancar una sesión se compara:

- Imagen ausente → error: `run 'geli --build-image' first`, y salir.
- Hash distinto → aviso en stderr sugiriendo reconstruir, pero **arranca igual** (lo acordado).

### CLI y setup.sh

- `Cli` gana `#[arg(long)] build_image: bool`, atendido en `main()` junto a `--list`.
- `setup.sh` llama a `geli --build-image` tras instalar el binario, y avisa de que ese paso tarda
  unos minutos una sola vez.

## Ficheros

- `src/main.rs` — todo lo anterior.
- `setup.sh` — invocar `--build-image`.
- `README.md` / `CLAUDE.md` — actualizar tiempos de arranque, el nuevo subcomando, la disposición
  de imágenes, y tachar el punto 2 del roadmap.

## Verificación

1. `cargo test` y `cargo clippy --all-targets` limpios. Tests nuevos: `golden_cloud_init_parses`,
   que el cloud-init de sesión ya **no** contenga `apt-get`/`npm install`, y que el hash de receta
   cambie al cambiar la receta.
2. `geli --build-image` → produce `geli-golden.qcow2` y `geli-golden.recipe`, apaga solo.
3. **Medir** una sesión con `time geli echo OK` y compararla con los ~4,5 min actuales. Es el
   objetivo del paso; si no baja de forma clara, el paso ha fallado.
4. Reutilizar el `check.sh` del paso 1 (montaje 9p, `.bash_profile` vs `.bashrc`, subshell
   anidado, TTY, `pwd`) contra la golden, más `which claude` y `which git`.
5. Borrar `geli-golden.qcow2` → confirmar el error con instrucción. Corromper
   `geli-golden.recipe` → confirmar que avisa pero arranca.

## Riesgos

- **El principal: que cloud-init no vuelva a ejecutarse** sobre una imagen donde ya corrió. Se
  ataca por dos vías independientes: `cloud-init clean` al hornear, e `instance-id` único por
  sesión (ya implementado en el paso 1). Si aun así no re-ejecuta, es el punto donde se cae el
  plan entero, así que se verifica antes que nada en el punto 3.
- `cloud-init clean` corre *dentro* de la propia ejecución de cloud-init. Es la receta habitual
  para preparar imágenes, pero si da problemas se retira y se confía solo en el `instance-id`.
- La cadena qcow2 guarda rutas absolutas: mover `~/qemu-sandbox/` rompe la golden. Se documenta;
  la solución es reconstruir.
- La versión de `claude-code` queda congelada hasta reconstruir. Lo cubre el aviso de receta
  obsoleta solo si cambia la receta, no si cambia el paquete upstream — conviene documentar que
  `--build-image` es también la forma de actualizar el agente.

---

## Resultado (verificado)

Implementado y verificado end-to-end.

**El riesgo principal no se materializó:** cloud-init sí vuelve a ejecutarse sobre una imagen
donde ya había corrido, con `cloud-init clean` al hornear más `instance-id` único por sesión.

**Mejora de arranque: ~275 s → 14,4 s** (medido con `time`), unas 19 veces más rápido y bastante
por debajo del objetivo de ~30 s del plan.

| Comprobación | Resultado |
|---|---|
| `cargo test` | 12/12 |
| `cargo clippy --all-targets` | 0 warnings |
| `geli --build-image` | `geli-golden.qcow2` (2,1 GB) + receta `537fd40babc9f2ff`, marcador `GELI_GOLDEN_OK` presente |
| Tiempo de sesión | **14,4 s** (antes ~275 s) |
| `claude` en la imagen | `/usr/local/bin/claude` |
| `git` en la imagen | `/usr/bin/git` |
| Montaje 9p | `CHECK_SHARED=marker file` |
| `.bash_profile` vs `.bashrc` | `CHECK_BASHRC=0`, `CHECK_PROFILE=3` |
| Subshell anidado | `CHECK_NESTED_OK` + `CHECK_AFTER_NESTED` |
| TTY real | `CHECK_TTY_OK` |
| Directorio de trabajo | `CHECK_PWD=/workspace/geli-e2e` |
| Imagen ausente | error con instrucción, exit code 1 |
| Receta obsoleta | avisa y arranca igual |

Extra no previsto: la construcción se hace sobre `geli-golden.qcow2.building` y solo se renombra
al fichero final si el invitado confirma que todas las herramientas están presentes, de modo que
un build fallido nunca sustituye a una imagen que funcionaba.
