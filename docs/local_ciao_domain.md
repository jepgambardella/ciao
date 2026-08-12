Aggiungi a CiaoShip una modalità di sviluppo locale integrata basata su domini **`.ciao`** e porte automatiche.

La UX desiderata è:

```bash
ciaoship dev
```

CiaoShip rileva automaticamente il nome del progetto dalla cartella, dai metadata del progetto o da `ciaoship.toml` e lo espone come:

```text
my-api.ciao
dashboard.ciao
blog.ciao
```

Deve essere possibile fare override:

```bash
ciaoship dev --name admin
```

ottenendo:

```text
http://admin.ciao
```

La configurazione opzionale è:

```toml
[dev]
name = "admin"
port = 41001
command = "bun run dev"
```

## Resolver `.ciao`

CiaoShip deve configurare automaticamente il sistema locale affinché:

```text
*.ciao → 127.0.0.1
```

La configurazione del resolver può richiedere privilegi amministrativi la prima volta, ma deve essere **automatica e one-time**.

Supportare almeno:

```text
macOS Apple Silicon
macOS Intel
Linux
```

Dopo il setup iniziale, `ciaoship dev` non deve richiedere ulteriori configurazioni DNS.

Non modificare `/etc/hosts` per ogni progetto: il resolver deve gestire l'intero namespace `*.ciao`.

## Porte automatiche

Principio:

> **Stable names. Disposable ports.**

CiaoShip mantiene stabile il dominio e considera la porta un dettaglio interno.

Esempio:

```text
my-api.ciao     → 127.0.0.1:41001
dashboard.ciao  → 127.0.0.1:41002
blog.ciao       → 127.0.0.1:41003
```

La porta deve essere scelta automaticamente.

Se il progetto indica una porta preferita e quella porta è libera, può essere utilizzata.

Se è occupata, **non deve mai essere un errore bloccante**: CiaoShip cerca automaticamente la successiva porta libera o un'altra porta disponibile nel range gestito.

Più progetti devono poter girare contemporaneamente senza conflitti.

CiaoShip mantiene internamente il mapping:

```text
project name → internal port
```

e un reverse proxy locale instrada:

```text
Host: my-api.ciao
→ 127.0.0.1:<porta assegnata>
```

Il mapping deve poter cambiare tra esecuzioni senza cambiare l'URL pubblico locale.

## Output desiderato

```bash
cd my-api
ciaoship dev
```

```text
✓ project detected: Bun
✓ local domain: my-api.ciao
✓ internal port: 41003
✓ resolver: .ciao active

http://my-api.ciao

Ready.
```

Riusa il più possibile il core esistente di CiaoShip:

```text
project detection
port allocation
process lifecycle
configuration
logging
```

Non creare un secondo sistema parallelo.

La feature deve rimanere piccola, automatica e coerente con la filosofia di CiaoShip:

> **Ship apps. Skip the ops.**

L'implementazione usa dnsmasq per il resolver e Caddy per il reverse proxy,
installandoli automaticamente con il package manager nativo al primo
`ciaoship dev`. Su macOS viene rilevato Homebrew nelle posizioni standard Apple
Silicon e Intel e viene installato se assente; su Linux sono supportati apt e
systemd-resolved. Il setup è idempotente e non modifica `/etc/hosts` per
progetto.
