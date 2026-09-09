# Recette : Logs lisibles pour un humain

**Statut** : À tester

**Specs** : [`20260909145940-logs-lisibles-humain-specs.md`](../specs/20260909145940-logs-lisibles-humain-specs.md)
**Plan** : [`20260909145940-logs-lisibles-humain-plan.md`](../plans/20260909145940-logs-lisibles-humain-plan.md)

Cahier **manuel** pour l'**utilisateur**, une fois les tâches concernées en `🧪 À recetter`. Init `À tester` / `—`. Guidé par `/imp-recette`. Seul l'utilisateur coche (`Testé` + date `JJ/MM/AAAA`). `✅ Recetté` ≠ cocher le cahier.

> Une modification des specs ou du plan entraîne **obligatoirement** une révision de cette recette.

## T1 — Lire le déroulé dans le terminal

**Objectif** : je veux que je comprenne ce qui se passe si je lance oxidrive et je regarde le terminal

### R1.1 — Chronologie d’une sync avec `--verbose`

| Statut | Date | Specs | Plan |
|--------|------|-------|------|
| À tester | — | T1 | T1.1, T1.2 |

1. **Contexte** — Config valide, compte déjà authentifié, terminal interactif, pas `--quiet`.
2. **Faire** — Lancer une sync unique avec `--verbose` (`oxidrive sync --once --verbose`).
3. **Puis faire** — Lire stderr pendant et après le cycle.
4. **Il doit se passer** — Le cycle s’exécute comme d’habitude.
5. **Je dois voir** — Des lignes façon Git : une phrase (pas d’horloge, pas de `INFO`). Je reconnais le début, le scan, l’exécution et un bilan. Les avertissements commencent par `warning:`. **Sans** l’ancien format (`15:04:05 INFO`, `paths=12 computing…`).

### R1.2 — Avertissement visible sans `--verbose`

| Statut | Date | Specs | Plan |
|--------|------|-------|------|
| À tester | — | T1 | T1.2 |

1. **Contexte** — Même install, **sans** `--verbose`. Provoker un cas qui produit un avertissement (erreur de transfert, watcher, etc.).
2. **Faire** — Relancer la commande qui déclenche l’avertissement (défaut quiet).
3. **Puis faire** — Repérer la ligne dans le terminal.
4. **Il doit se passer** — Le programme ne masque pas le problème.
5. **Je dois voir** — Une ligne `warning:` ou `error:` façon Git : phrase courte, éventuellement `'chemin'`. Pas de dump `file_id:` / `error:` / `owner:` sur la même ligne. Les étapes de sync restent absentes.

### R1.3 — Redirection sans codes couleur

| Statut | Date | Specs | Plan |
|--------|------|-------|------|
| À tester | — | T1 | T1.1 |

1. **Contexte** — Même config.
2. **Faire** — Relancer `oxidrive sync --once --verbose` en redirigeant stderr vers un fichier (`2> /tmp/oxidrive-stderr.txt` ou équivalent).
3. **Puis faire** — Ouvrir ce fichier dans un éditeur texte brut.
4. **Il doit se passer** — Le cycle s’exécute.
5. **Je dois voir** — Le même type de phrases qu’au R1.1 (forme Git), **sans** séquences d’échappement couleur (pas de `^[[` / codes ANSI).

### R1.4 — Défaut quiet, `--verbose` ajoute des lignes moins importantes

| Statut | Date | Specs | Plan |
|--------|------|-------|------|
| À tester | — | T1 | T1.1, T1.3 |

1. **Contexte** — Sync qui, avec `--verbose`, affiche plusieurs lignes d’étapes et un bilan (R1.1).
2. **Faire** — Relancer **sans** `--verbose` ni `--quiet` (défaut).
3. **Puis faire** — Relancer avec `--verbose`, puis éventuellement `--verbose --verbose`.
4. **Il doit se passer** — Le défaut est silencieux sur le succès ; `--verbose` ajoute des lignes, sans changer de style.
5. **Je dois voir** — Au défaut : pas d’étapes ni de bilan, **ni sur stderr ni sur stdout** (pas de `Using device id` / `Sync complete`). Seulement `warning:` / `error:` s’il y en a. Avec `--verbose` : plus de lignes **moins importantes**, même présentation façon Git — **pas** l’ancien dump. Avec `--verbose` répété : encore plus **côté oxidrive** (pas un dump hyper/reqwest), toujours ce format. `--quiet` équivaut au défaut.

## T2 — Consulter le fichier de log

**Objectif** : je veux que je comprenne aussi l’historique si j’ouvre le fichier de log configuré

### R2.1 — Le fichier reste du JSON

| Statut | Date | Specs | Plan |
|--------|------|-------|------|
| À tester | — | T2 | T2.1, T2.2 |

1. **Contexte** — `log_file` renseigné dans la config, répertoire parent accessible.
2. **Faire** — Lancer une sync unique (`oxidrive sync --once`).
3. **Puis faire** — Ouvrir le fichier de log dans un éditeur, et tenter de parser une ligne (JSON).
4. **Il doit se passer** — Des lignes sont écrites pour le cycle.
5. **Je dois voir** — Chaque ligne est du JSON (accolades, champs), **pas** le format une-ligne du terminal. Pas de codes couleur. Le terminal (R1.1) reste lisible à part.

## T3 — Savoir à quoi s’attendre

**Objectif** : je veux que la doc me dise comment se présentent les logs si je configure ou lance oxidrive

### R3.1 — README et exemple de configuration

| Statut | Date | Specs | Plan |
|--------|------|-------|------|
| À tester | — | T3 | T3.1 |

1. **Contexte** — Dépôt à jour après implémentation.
2. **Faire** — Lire la section usage du README (flags globaux) et le bloc Logging de `config.example.toml`.
3. **Puis faire** — Comparer avec ce que R1.1 et R2.1 ont montré.
4. **Il doit se passer** — Aucune instruction contredisant le comportement réel.
5. **Je dois voir** — Quiet par défaut, `--verbose` = plus de lignes lisibles façon Git (pas l’ancien dump), `--quiet`, fichier JSON.
