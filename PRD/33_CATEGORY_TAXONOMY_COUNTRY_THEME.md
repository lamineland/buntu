# PRD 33 — Catégories normalisées : regroupement par pays et par thème

- **ID :** `PRD-33`
- **Prérequis :**
  - `PRD-02` (ingestion M3U en flux, normaliseur) ;
  - `PRD-03` (client Xtream, catégories `get_*_categories`) ;
  - `PRD-05` (requêtes catalogue, facettes `get_groups`) ;
  - `PRD-09` / `PRD-10` (écrans Live TV et VOD, `CategoryNavigation`) ;
  - `PRD-13` (assistant ⌘K, résolution locale) ;
  - `PRD-20` (bouquets masqués, ordre personnalisé, profils enfants, catégories adultes).
- **Couches impactées :**
  - `Rust (crates/cnd-core/src/taxonomy/ (nouveau), iptv/, db/)` ;
  - `UniFFI` ;
  - `SwiftUI (Shared/CategoryNavigation, Features/LiveTV, Features/VOD, Features/Profiles)`.
- **Plateformes :** toutes (macOS, iPhone, iPad, Apple TV, Vision Pro).
- **Objectifs SLA :**
  - **Classement :** < 5 µs par élément ; l'ingestion de 50 000 éléments reste **< 1,5 s** (SLA du `PRD-02`) avec le classement inclus.
  - **Facettes :** liste des pays ou des thèmes avec compteurs, filtres croisés compris, **< 50 ms** sur 50 000 éléments.
  - **Reclassement** d'une base existante (migration ou mise à jour du dictionnaire) : **< 2 s** pour 50 000 éléments, sans retélécharger les sources.

---

## 1. Contexte & Objectif

### 1.1. Le problème

La colonne des catégories affiche aujourd'hui la valeur brute `group_title` de chaque élément, regroupée telle quelle (`get_groups`, `catalog_repo.rs`). Sur la base locale actuelle (deux playlists M3U, 12 229 chaînes) cela donne **242 catégories** de Live TV, dont :

- **453 chaînes dans des combinaisons** à point-virgule : `Animation;Kids`, `Animation;Kids;Music`, `Animation;Classic;Entertainment`… Chaque combinaison devient une catégorie à part, souvent avec **1 seule chaîne**. « Kids » existe en réalité dans 437 chaînes, réparties sur une dizaine de lignes.
- **Deux axes mélangés dans la même liste** : la playlist 1 range par thème (`News`, `Sports`, `Music`), la playlist 2 par pays (`Italy`, `Poland`, `UK`). On lit donc « Albania, Andorra, Animation, Animation;Classic, Argentina… » dans un seul ordre alphabétique.
- **Des valeurs vides de sens** : `Undefined` (1 547 chaînes), `General` (2 673).
- **Le pays est presque toujours connu, mais pas utilisé** : la colonne `country` n'est remplie que pour 1 907 chaînes sur 12 229, alors que **10 233 chaînes de la playlist 1 ont un `tvg-id` qui porte le pays** (`CannesLerinsTV.fr@SD`, `DHA.tr@SD`, `OberpfalzTV.de@SD`).
- **Les fournisseurs Xtream** ont leurs propres conventions (`FR| SPORTS`, `|UK| ENTERTAINMENT FHD`, `AR - MBC`, `VIP | BEIN SPORTS`) : pays et thème sont collés dans un même libellé, avec des préfixes et des mentions de qualité.

### 1.2. L'objectif

Chaque élément du catalogue reçoit, **à l'ingestion**, deux informations normalisées et stables :

1. **Un pays** : code ISO 3166-1 alpha-2 (`FR`, `GB`, `US`…), ou `INT` (international / multi-pays), ou rien si inconnu.
2. **Un ou plusieurs thèmes** pris dans une **liste fermée** (§ 3), chacun avec un sous-thème facultatif (`kids/animation`, `lifestyle/cooking`).

L'interface propose alors trois façons de parcourir, avec filtres croisés :

- **Par pays** (« France » → chips de thème : Infos 30, Sport 12, Jeunesse 8…) ;
- **Par thème** (« Sport » → chips de pays : France, Royaume-Uni, Italie…) ;
- **Catégories du fournisseur** : la liste brute actuelle, inchangée, pour ne rien perdre.

La valeur brute `group_title` **n'est jamais modifiée** : bouquets masqués, ordre personnalisé, profils enfants et Xtream continuent de fonctionner dessus.

---

## 2. Principes d'architecture (une responsabilité par module)

| Module | Seule responsabilité | Ne fait pas |
|---|---|---|
| `taxonomy/data/countries.rs` | Table des pays : code ISO, alias (EN, FR, langue locale, abréviations `UK`, `USA`), TLD. Données pures, générées. | Aucune logique. |
| `taxonomy/data/themes.rs` | Liste fermée des thèmes et sous-thèmes, et dictionnaire de mots-clés multilingue → thème. Données pures. | Aucune logique. |
| `taxonomy/tokenize.rs` | Découper un libellé de catégorie en segments et mots (`;`, `|`, `/`, ` - `, `[ ]`, `( )`), retirer le bruit (`FHD`, `HD`, `4K`, `VIP`, `24/7`, émojis, drapeaux). | Ne connaît ni pays ni thèmes. |
| `taxonomy/country.rs` | Déduire le pays d'un élément à partir des indices (§ 4.1), avec sa provenance. | Ne touche pas aux thèmes. |
| `taxonomy/theme.rs` | Déduire les thèmes d'un élément à partir des indices (§ 4.2), avec leur provenance. | Ne touche pas au pays. |
| `taxonomy/mod.rs` | Façade `classify(&ClassifyInput) -> Classification` : appelle `country` et `theme`, applique les surcharges de l'utilisateur. | Pas d'accès SQLite. |
| `db/taxonomy_repo.rs` | Écrire et lire les classements, facettes et filtres croisés ; reclassement en masse. | Pas de règle de classement. |
| `lib.rs` | Exports UniFFI. | — |
| `Shared/CategoryNavigation.swift` | Afficher l'axe choisi et les chips de l'autre axe. | Aucun calcul de classement côté Swift. |

Le classement est une **fonction pure** (mêmes entrées → même résultat), testable sans base de données, et versionnée (`TAXONOMY_VERSION`) pour savoir quand reclasser.

---

## 3. Référentiel des thèmes (liste fermée)

Identifiants stables en anglais (stockés), libellés traduits dans l'interface.

| `theme` | Libellé FR | Sous-thèmes (`subtheme`) | Exemples de libellés source reconnus |
|---|---|---|---|
| `general` | Généralistes | — | General, Généraliste, Generalista |
| `news` | Infos | `business`, `weather`, `politics` | News, Info, Actualités, Noticias, Notizie, Nachrichten, أخبار, Business, Weather, Météo |
| `sports` | Sport | `football`, `motorsport`, `combat`, `outdoor` | Sports, Sport, Deportes, beIN, Foot, Racing, Fight |
| `entertainment` | Divertissement | `comedy`, `reality`, `interactive` | Entertainment, Divertissement, Comedy, Humour |
| `movies` | Cinéma | `classic` | Movies, Films, Cinéma, Cine, Classic |
| `series` | Séries | — | Series, Séries, Feuilletons, Novelas |
| `kids` | Jeunesse | `animation`, `family` | Kids, Enfants, Jeunesse, Cartoon, Animation, Family, Infantil |
| `music` | Musique | `relax` | Music, Musique, Música, Clips, Radio |
| `documentary` | Documentaires | `science`, `culture`, `nature`, `history` | Documentary, Docs, Science, Culture, Discovery |
| `lifestyle` | Art de vivre | `cooking`, `travel`, `auto`, `fashion` | Lifestyle, Cooking, Cuisine, Travel, Auto, Outdoor |
| `education` | Éducation | — | Education, Educativo |
| `religious` | Religion | — | Religious, Religion, Islam, Christian, Quran |
| `institutional` | Institutionnel | `legislative`, `public` | Legislative, Parlement, Public |
| `shopping` | Téléachat | — | Shop, Shopping, Téléachat |
| `adult` | Adultes | — | XXX, Adult, +18 (règle actuelle `is_adult_category`) |
| `other` | Non classé | — | Undefined, Other, Divers, aucun indice |

Correspondance des 28 catégories iptv-org relevées dans la base : *General → general ; News, Business, Weather → news ; Sports → sports ; Outdoor → lifestyle/outdoor ; Entertainment, Comedy, Interactive → entertainment ; Movies, Classic → movies ; Series → series ; Kids, Animation, Family → kids ; Music, Relax → music ; Documentary, Science, Culture → documentary ; Lifestyle, Cooking, Travel, Auto → lifestyle ; Education → education ; Religious → religious ; Legislative, Public → institutional ; Shop → shopping ; Undefined → other.*

Un élément peut avoir **plusieurs thèmes** (`Animation;Kids;Music` → `kids/animation` + `music`). Un **thème principal** est retenu pour le tri et l'affichage : le premier segment reconnu, sauf `general` et `other` qui cèdent la place à tout autre thème.

---

## 4. Règles de classement

### 4.1. Pays (première règle qui répond, provenance enregistrée)

| Ordre | Indice | Exemple | Provenance |
|---|---|---|---|
| 0 | Surcharge de l'utilisateur (§ 7) | — | `user` |
| 1 | Attribut M3U `tvg-country` / `country` | `tvg-country="FR"` | `attribute` |
| 2 | Préfixe pays du libellé de catégorie | `FR| SPORTS`, `|UK| …`, `AR - MBC`, `[DE] …` | `group_prefix` |
| 3 | Libellé de catégorie = nom de pays (alias multilingues) | `Italy`, `Royaume-Uni`, `UK`, `USA`, `Deutschland` | `group_name` |
| 4 | Suffixe pays du `tvg-id` | `DHA.tr@SD` → `TR`, `BBCOne.uk` → `GB` | `tvg_id` |
| 5 | Préfixe pays du nom de la chaîne | `|FR| TF1 FHD`, `FR: M6` | `name_prefix` |
| 6 | Indice international | `International`, `World`, `Worldwide` | `INT` |
| — | Aucun indice | — | pays vide (« Pays inconnu ») |

- **Codes ambigus refusés** : `.tv`, `.com`, `.net`, `.org` ne sont pas des pays ; `uk` → `GB` ; un préfixe de 2 lettres n'est accepté que s'il est suivi d'un séparateur (`|`, `:`, ` - `, `]`) et figure dans la table des pays.
- **Langue ≠ pays** : `tvg-language` ne sert pas à déduire un pays (l'arabe ou l'espagnol couvrent trop de pays). Il est conservé pour un futur filtre « Langue » (§ 11).
- **Xtream** : le pays de la catégorie (`category_name`) s'applique à tous ses éléments ; les règles 2, 3 et 6 sont évaluées une seule fois par catégorie.

### 4.2. Thèmes

1. **Surcharge de l'utilisateur** (§ 7).
2. **Segments du libellé de catégorie** après retrait du préfixe pays et du bruit : chaque segment est comparé au dictionnaire (mot exact, puis racine sans accents) → 0 à n thèmes. Provenance `group`.
3. **Type d'élément** : un film sans thème reconnu reçoit `movies`, un épisode `series`. Provenance `kind`.
4. **Nom de la chaîne** (indice faible, seulement si 2 et 3 n'ont rien donné) : mots isolés comme `Sport`, `News`, `Info`, `Kids`, `Junior`, `Music`, `Cinema`. Provenance `name`.
5. Sinon `other`.

- Un libellé qui n'est **que** un pays (`Italy`) ne donne aucun thème : la chaîne passe par l'étape 4.
- `adult` réutilise la règle actuelle `is_adult_category` pour ne pas changer le contrôle parental (§ 6).
- **Lot ultérieur (33.5)** : les catégories XMLTV (`<category>` des programmes) d'une chaîne peuvent proposer un thème quand les étapes 2 à 4 échouent.

### 4.3. Dictionnaire et version

- Dictionnaire **multilingue** : anglais, français, espagnol, italien, allemand, portugais, turc, arabe ; extensible sans changer le code (fichier de données dans la crate).
- Toute modification du dictionnaire ou des règles **incrémente `TAXONOMY_VERSION`**. Au démarrage, si la version stockée est plus ancienne, la base est **reclassée en arrière-plan** à partir des colonnes déjà stockées (§ 5.3), sans retélécharger les sources.

---

## 5. Données (SQLite)

### 5.1. Schéma

```sql
-- Pays et thème principal sur l'élément : facettes et tri sans jointure.
ALTER TABLE iptv_items ADD COLUMN tx_country TEXT;          -- 'FR', 'INT' ou NULL
ALTER TABLE iptv_items ADD COLUMN tx_country_src TEXT;      -- 'attribute' | 'group_prefix' | … | 'user'
ALTER TABLE iptv_items ADD COLUMN tx_theme TEXT;           -- thème principal
ALTER TABLE series_catalog ADD COLUMN tx_country TEXT;
ALTER TABLE series_catalog ADD COLUMN tx_theme TEXT;

-- Tous les thèmes d'un élément (multi-valué).
CREATE TABLE IF NOT EXISTS item_themes (
    item_id INTEGER NOT NULL REFERENCES iptv_items(id) ON DELETE CASCADE,
    theme TEXT NOT NULL,
    subtheme TEXT,
    src TEXT NOT NULL,
    PRIMARY KEY (item_id, theme)
) WITHOUT ROWID;

-- Surcharges de l'utilisateur, par stable_hash pour survivre aux actualisations.
CREATE TABLE IF NOT EXISTS taxonomy_overrides (
    stable_hash TEXT PRIMARY KEY,
    country TEXT,
    themes TEXT,           -- liste 'sports,news' ; NULL = pas de surcharge
    updated_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_items_kind_country_theme ON iptv_items(kind, tx_country, tx_theme, name);
CREATE INDEX IF NOT EXISTS idx_item_themes_theme ON item_themes(theme, item_id);
```

- Migrations ajoutées à `apply_incremental_migrations` (`db/mod.rs`), qui ignore déjà les colonnes existantes.
- `TAXONOMY_VERSION` stockée dans `app_settings`.

### 5.2. Ingestion

- Le classement est une **passe de fin d'actualisation** : `classify_source(source_id)` (`db/taxonomy_repo.rs`) relit les éléments enregistrés de la source, les classe par pages de 5 000 (une transaction par page) et écrit `tx_*` et `item_themes`. Une seule passe sert M3U, Xtream (direct, films, séries) et le repli M3U+ ; les écritures de l'ingestion (`ingestion_writer.rs`, `iptv/xtream.rs`) restent inchangées.
- Appels : fin de `ingest_m3u_reader`, fin de `sync_xtream_native_catalog`, et `classify_series_episodes` après le chargement des épisodes d'une série Xtream. Un échec de classement est journalisé (`TAXONOMY`) et **ne fait pas échouer** l'actualisation.
- Le classement par `category_id` Xtream n'est pas mis en cache : à moins de 5 µs par élément, ce n'est pas nécessaire.
- **Thème emprunté** (`borrow_themes_by_tvg_id`), après chaque passe : une chaîne en direct dont le thème est `other` prend le thème le plus fréquent des chaînes de même `tvg-id` (sans le suffixe `@SD`) qui en ont un vrai (ni `other`, `general` ni `adult`). Provenance `tvg_id`. Une chaîne corrigée par l'utilisateur n'est jamais touchée.

### 5.3. Reclassement

- `reclassify_all(progress, cancel)` lit `group_title`, `tvg_id`, `name`, `country`, `kind` par pages de 5 000 et réécrit les colonnes `tx_*` et `item_themes`. Annulable, rapporte sa progression ; la version n'est enregistrée que si elle va au bout.
- Déclenché par : la première ouverture après la migration et tout changement de `TAXONOMY_VERSION` (`ensure_taxonomy_current`, **dans un fil d'arrière-plan** lancé par `CndEngine`, `is_reclassifying_categories()` dit s'il tourne) ; une surcharge de l'utilisateur (l'élément seul).

### 5.4. Requêtes et API UniFFI (extrait)

```rust
enum CategoryAxis { Country, Theme, Provider }

fn get_category_facets(kind: String, axis: CategoryAxis, source_id: Option<i64>,
                       filter: TaxonomyFilter) -> Vec<CategoryFacet>;
// kind = 'live' | 'movie' | 'series'. Axe Pays : filtré par le thème choisi seulement ; axe Thèmes : par le pays seulement.
// CategoryFacet { key: String, count: u32, subkeys: Vec<FacetCount> }
//   axis Country : key = 'FR' | 'INT' | '' (inconnu)
//   axis Theme  : key = 'sports', subkeys = sous-thèmes
//   axis Provider: key = group_title brut (comportement actuel de get_groups)

// Pas de `get_items_by_taxonomy` : les requêtes existantes reçoivent le filtre, ce qui garde la
// recherche plein texte, le tri, les favoris et les règles de visibilité.
fn query_catalog_by_taxonomy(filter: CatalogFilter, taxonomy: TaxonomyFilter) -> Vec<IptvItemRecord>;
fn query_series_by_taxonomy(filter: CatalogFilter, taxonomy: TaxonomyFilter) -> Vec<SeriesRecord>;
// TaxonomyFilter { country: Option<String> /* '' = pays inconnu */, theme: Option<String>, subtheme: Option<String> }

fn set_taxonomy_override(stable_hash: String, country: Option<String>, themes: Option<Vec<String>>);
fn clear_taxonomy_override(stable_hash: String);
fn taxonomy_status() -> TaxonomyStatus; // version du code et version stockée, chaînes en direct sans pays / sans thème
fn is_reclassifying_categories() -> bool;
```

- `get_groups` reste en place et sert l'axe `Provider` : aucun appel existant ne casse. `CatalogFilter` n'est pas modifié (pas de rupture côté Swift).
- **Séries** : `series_catalog` porte `tx_country` et `tx_theme` (thème principal seulement, pas de sous-thème). Les valeurs du filtre sont validées (code pays de 2 lettres ou `INT`, thème de la liste fermée, sous-thème en minuscules) puis écrites dans le SQL ; toute autre valeur donne un résultat vide, jamais une requête modifiée.
- Les filtres « masqué », « adulte » et « profil enfant » du `PRD-20` sont appliqués à l'identique dans les nouvelles requêtes.

---

## 6. Interactions avec l'existant

- **Bouquets masqués et ordre personnalisé (`PRD-20`)** : restent sur `group_title` (axe Fournisseur). Lot 33.4 : masquer aussi un pays ou un thème entier (`hidden_groups.kind` = `country:live` / `theme:live`), et ordre personnalisé des pays (France en premier par défaut selon la région de l'appareil).
- **Catégories adultes** : un élément est adulte si `is_adult_group(group_title)` **ou** s'il porte le thème `adult`. Rien ne devient visible qui était masqué.
- **Profils enfants** : en plus des catégories autorisées, un parent peut autoriser des **thèmes** (`kids`, `education`, `documentary`). Le thème `adult` ne peut jamais être autorisé.
- **Recherche (FTS5)** : les libellés traduits du pays et du thème principal sont ajoutés à l'index (« sport italie » trouve les chaînes sportives italiennes). Changement de schéma FTS : reconstruction de l'index au premier lancement.
- **Assistant ⌘K (`PRD-13`)** : « une chaîne d'info en anglais », « le sport en Espagne » utilisent `tx_theme` et `tx_country` au lieu de mots cherchés dans `group_title`.
- **Recommandations (`PRD-27`)** et **miroirs (`PRD-17`)** : peuvent s'appuyer sur `tx_country` pour départager des homonymes ; hors périmètre de ce PRD, simple possibilité.
- **iCloud** : le catalogue n'est pas synchronisé ; les surcharges de l'utilisateur non plus dans ce PRD (§ 11).

---

## 7. Corrections par l'utilisateur

- Menu contextuel d'une chaîne : « Changer le pays… », « Changer le thème… » (liste fermée, recherche). L'élément est reclassé immédiatement ; la surcharge survit aux actualisations (clé `stable_hash`).
- Menu contextuel d'une catégorie fournisseur : « Classer cette catégorie… » applique la surcharge à tous ses éléments (stockée comme règle sur `group_title` normalisé, table `taxonomy_group_overrides`, lot 33.4).
- Réglages > Catalogue : « Réinitialiser mes corrections de catégories ».

---

## 8. Interface (SwiftUI)

### 8.1. Colonne des catégories

- En tête de `CategoryNavigation`, un sélecteur segmenté : **Pays · Thèmes · Fournisseur**. Le choix est mémorisé par écran (`@AppStorage`). Valeur par défaut : **Thèmes** en Live TV, en Films et en Séries (lot 33.5).
- **Axe Pays** : drapeau (émoji régional calculé depuis le code), nom localisé par `Locale.current.localizedString(forRegionCode:)`, compteur. En tête : « Tous les pays », puis le pays de l'appareil, puis l'ordre alphabétique localisé ; « International » et « Pays inconnu » en fin de liste.
- **Axe Thèmes** : icône SF Symbol par thème, libellé, compteur ; un thème se déplie sur ses sous-thèmes. « Non classé » en fin de liste.
- **Axe Fournisseur** : liste actuelle, inchangée.
- **Plus de libellés bruts à point-virgule** dans les axes Pays et Thèmes.

### 8.2. Filtre croisé

- Quand un pays est choisi, une rangée de **chips de thèmes** (avec compteurs) apparaît au-dessus de la grille ; et inversement. Exemple : Pays = France → [Tous 214] [Infos 30] [Sport 12] [Jeunesse 8]…
- Les chips à 0 sont masquées. La sélection apparaît dans le titre (« Sport · France »).

### 8.3. Petits écrans et Apple TV

- iPhone : les deux axes dans la feuille « Catégories » existante, en sections.
- Apple TV : le sélecteur d'axe est une rangée focusable au-dessus de la colonne (`focusSection`).
- Jetons de design et règles d'accessibilité des `PRD-30` et `PRD-31` (libellés VoiceOver « France, 214 chaînes »).

---

## 9. Lots

| Lot | Contenu | Livrable validable seul |
|---|---|---|
| **33.1** | Module `taxonomy/` : données pays et thèmes, `tokenize`, `country`, `theme`, `classify`. Aucun branchement. | Tests unitaires sur un **jeu de libellés réels** (les 242 catégories de la base locale, des libellés Xtream courants, des `tvg-id` iptv-org) avec le résultat attendu. |
| **33.2** ✅ | Schéma, migrations, classement en fin d'actualisation (M3U et Xtream), reclassement en arrière-plan, **thème emprunté à une autre chaîne de même `tvg-id`** quand le libellé n'en donne pas (502 chaînes de la base locale), requêtes de facettes et filtres croisés, exports UniFFI, XCFramework. | Tests Rust de bout en bout et SLA ; `get_groups` inchangé. |
| **33.3** ✅ | Interface : sélecteur d'axe, colonnes Pays et Thèmes, chips croisées, titres, iPhone et Apple TV. | Essai à l'écran sur la base locale. |
| **33.4** ✅ | Corrections de l'utilisateur, masquage de pays / thèmes, thèmes autorisés des profils enfants, recherche FTS par pays et thème, assistant ⌘K. | Tests Rust et Swift ; essai à l'écran. |
| **33.5** | VOD : genres de films et séries (même moteur, sous-thèmes `comedy`, `family`, `classic`, `action`…), indice XMLTV `<category>` pour les chaînes non classées. | Tests et essai à l'écran. |

---

## 10. Fichiers à Créer / Modifier

- **Rust :**
  - `crates/cnd-core/src/taxonomy/` *(nouveau)* : `mod.rs`, `tokenize.rs`, `country.rs`, `theme.rs`, `data/countries.rs`, `data/themes.rs`, `tests.rs` + `tests/fixtures/category_labels.tsv` ;
  - `db/schema.rs`, `db/mod.rs` *(migrations)*, `db/ingestion_writer.rs`, `db/taxonomy_repo.rs` *(nouveau)*, `db/catalog_repo.rs` *(filtres partagés)* ;
  - `iptv/xtream.rs` *(classement par catégorie)* ;
  - `ai/local_matcher.rs`, `ai/candidates.rs` *(lot 33.4)* ;
  - `lib.rs` *(exports UniFFI)*, puis `task xcframework`.
- **Swift :**
  - `Shared/CategoryNavigation.swift` *(axe, chips croisées)*, `Shared/CategoryAxis.swift` *(nouveau : libellés, icônes, drapeaux)* ;
  - `Features/LiveTV/LiveTvModel.swift`, `Features/LiveTV/LiveTvView.swift` ;
  - `Features/VOD/VodCatalogModel.swift` *(lot 33.5)* ;
  - `Features/Profiles/KidsAllowList.swift`, `BouquetEditorModel.swift` *(lot 33.4)*.
- **Tests :** `apple/Tests/CndIptvAppTests/CategoryAxis*Tests.swift`.
- **Index :** `PRD/00_INDEX_ROADMAP.md`.

---

## 11. Contraintes Non-Négociables (Guardrails IA)

- **`group_title` n'est jamais réécrit.** Les nouvelles informations vont dans des colonnes et tables séparées.
- **Aucune régression du contrôle parental** : tout élément masqué avant (adulte, catégorie masquée, profil enfant) le reste après, quel que soit l'axe. Test dédié obligatoire.
- **Classement 100 % local et déterministe** : aucun appel réseau, aucune IA pour classer. L'IA de l'utilisateur n'est pas sollicitée.
- **Liste de thèmes fermée** : aucun thème inventé à partir des données ; un libellé inconnu donne `other`, jamais un nouveau thème.
- **Pas de nouvelle dépendance lourde** : données ISO embarquées dans la crate (pas de crate de géographie, pas de fichier téléchargé).
- **SLA d'ingestion du `PRD-02` maintenu** (test de performance existant au vert).
- **Les cibles macOS, iOS, tvOS et visionOS restent vertes.**

---

## 12. Hors périmètre (PRD ultérieurs possibles)

- **Filtre par langue** (`tvg-language`, pistes audio) : troisième axe possible, mêmes mécanismes.
- **Synchronisation iCloud des corrections** de l'utilisateur.
- **Base de chaînes externe** (API iptv-org `channels.json`) pour enrichir pays et thèmes : apporterait de la précision mais un appel réseau et une dépendance de données.
- **Régions** (Maghreb, Afrique de l'Ouest, Amérique latine) comme regroupement de pays.

---

## 13. Critères d'Acceptation & Commandes de Validation

1. **Classement (lot 33.1), sur le jeu de libellés réels :**
   - `Animation;Kids;Music` → thèmes `kids/animation` + `music`, principal `kids` ;
   - `FR| SPORTS FHD` → `FR` + `sports` ; `|UK| ENTERTAINMENT` → `GB` + `entertainment` ; les préfixes ambigus (`AR`, `IN`) suivent la décision de la § 14 et ont chacun un cas dans les fixtures ;
   - `Italy` → `IT`, aucun thème issu du libellé ; `UK` → `GB` ; `USA` → `US` ;
   - `tvg-id="DHA.tr@SD"` → `TR` ; `BBCOne.uk` → `GB` ; `Foo.tv` → aucun pays ;
   - `Undefined` → `other` ; `General` → `general` ;
   - **≥ 95 %** des chaînes de la base locale ont un pays (lot 33.1) ;
   - **≤ 15 %** restent `other` **après les lots 33.2 et 33.5** : un libellé `Undefined` ou un simple nom de pays ne porte aucun thème, le classement seul ne peut pas faire mieux (mesure du lot 33.1 : 27,2 %).
2. **Base et requêtes (lot 33.2) :** migration d'une base existante, reclassement < 2 s pour 50 000 éléments, facettes < 50 ms, filtres croisés, surcharges conservées après actualisation d'une source, `get_groups` identique à avant.
3. **Contrôle parental :** test qui compare l'ensemble des éléments visibles avant et après, pour un profil adulte verrouillé et un profil enfant.
4. **Interface (lot 33.3) :** la colonne de la capture d'écran du ticket n'affiche plus de combinaison à point-virgule dans les axes Pays et Thèmes ; « France → Sport » et « Sport → France » donnent la même liste.
5. `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --release`, `cd apple && swift test`, `task check` au vert.

---

## 14. Questions ouvertes

- **Axe par défaut** en Live TV : Thèmes (proposé) ou Pays ? Pour un utilisateur qui ne regarde qu'un ou deux pays, « Pays » d'abord serait plus direct.
- **`Classic`** (iptv-org) : classé en `movies/classic` (proposé) ou `entertainment` ? Le contenu mélange films et séries anciennes.
- **`Animation` sans `Kids`** (`Animation;Comedy`, souvent pour adultes) : `kids/animation` ou `entertainment/comedy` ? Proposé : `entertainment` quand `Comedy` est présent sans `Kids`.
- **Préfixes Xtream à deux lettres ambigus** (`AR` = Argentine ou arabe, `IN` = Inde ou mot anglais) : faut-il une liste de préfixes « langue » (`AR`, `EN`) traités comme international, ou une détection par fournisseur ?
- **Masquer « Pays inconnu »** quand il est vide, ou le garder visible pour expliquer le classement ?

---

## 15. Statut d'Implémentation & Validation

**Lots 33.1 à 33.5 terminés** (branche `feat/category-taxonomy`).

- **Module `crates/cnd-core/src/taxonomy/`**, fonctions pures sans SQLite ni réseau, non branché sur l'ingestion :
  - `data/countries.rs` : 213 pays et territoires ISO avec noms anglais, français et locaux, codes non ISO (`uk`, `usa`, `uae`, `ksa`), préfixes de langue, mots « international » ;
  - `data/themes.rs` : dictionnaire de mots-clés en 8 langues vers les 16 thèmes, mots-clés des noms de chaînes, sous-thèmes « qualificatifs » ;
  - `tokenize.rs`, `country.rs`, `theme.rs`, `mod.rs` (`classify`, `classify_with_override`, `TAXONOMY_VERSION = 1`).
- **Décisions prises pour avancer** (questions de la § 14, modifiables) :
  - `Classic`, `Family` et `Interactive` qualifient un autre thème : `Classic;News` → Infos ; seuls, `Classic` → `movies/classic` et `Family` → `kids/family` ;
  - `Public` (iptv-org : chaîne de service public) → `general/public`, pas `institutional` ; `Legislative` reste `institutional` ;
  - `Animation;Comedy` sans `Kids` → `entertainment/comedy` ;
  - préfixes `AR`, `EN`, `LATINO`, `ARABIC` = langue, jamais un pays ; `IN |` = Inde ; `EXYU`, `AFRICA` = `INT` ;
  - suffixes de `tvg-id` ignorés : `.tv`, `.fm`, `.io`, `.cc`, `.ws` ; `.uk` → `GB`.
- **Tests :** 24 tests, dont un **fichier de référence relu** (`crates/cnd-core/tests/fixtures/category_labels.tsv`) : les 278 libellés réels de la base locale et 44 libellés Xtream courants avec pays et thèmes attendus. Mise à jour : `TAXONOMY_BLESS=1 cargo test -p cnd-core --lib taxonomy::tests`.
- **Mesures sur la base locale** (test ignoré `coverage_on_a_real_catalogue`, lecture seule) :
  - pays trouvé pour **99,9 %** des 12 229 chaînes (avant : 15,6 %) : 10 292 par le `tvg-id`, 1 907 par l'attribut, 15 par le nom de la catégorie ;
  - thème `other` pour **27,2 %** : 1 536 `Undefined` et 1 795 chaînes rangées sous un simple nom de pays. 502 de ces dernières ont un thème dans l'autre playlist par leur `tvg-id` (lot 33.2).
- **Validation :** `cargo clippy -p cnd-core --all-targets -- -D warnings` au vert ; `cargo test --workspace --release` : tous les tests du lot au vert.
  - **Hors lot, déjà rouge sur `main`** : `recommend::tests::test_recommendations_sla_50k_items` (55 ms pour 50 ms) ;
  - `test_50k_streaming_m3u_ingestion_sla` échoue quand toute la suite tourne en parallèle et passe seul, comme indiqué dans le README.

### Lot 33.2 : stockage, classement et requêtes

- **Schéma** (migrations dans `apply_incremental_migrations`, `db/mod.rs`) : `iptv_items.tx_country`, `tx_country_src`, `tx_theme` ; `series_catalog.tx_country`, `tx_theme` ; tables `item_themes` et `taxonomy_overrides` ; index `(kind, tx_country, tx_theme, name)` et `(theme, item_id)`.
- **`db/taxonomy_repo.rs`** : classement par pages, passe du thème emprunté, `reclassify_all`, `ensure_taxonomy_current`, surcharges (appliquées tout de suite, conservées par un reclassement complet), `get_category_facets`, `TaxonomyFilter` ; `query_catalog_with` et `query_series_with` dans `catalog_repo.rs` (les anciennes fonctions les appellent avec un filtre vide).
- **API UniFFI** (`lib.rs`) : `get_category_facets`, `query_catalog_by_taxonomy`, `query_series_by_taxonomy`, `set_taxonomy_override`, `clear_taxonomy_override`, `taxonomy_status`, `is_reclassifying_categories`.
- **Écarts avec le plan initial** (notés ci-dessus) : classement en fin d'actualisation et non pendant l'insertion ; filtre ajouté aux requêtes existantes au lieu de `get_items_by_taxonomy`.
- **Mesures sur une copie de la base locale** (12 229 chaînes) : migration et reclassement complet en **0,34 s** ; 180 pays et 14 thèmes dans la colonne, calculés en 11 et 12 ms ; 242 catégories fournisseur inchangées ; 15 chaînes sans pays ; thème `other` : 2 933 (27,2 % → 24,0 %, soit 398 chaînes qui ont emprunté un thème).
- **Mesures sur 50 000 éléments** (`--release`) : reclassement ≈ 1,1 s (objectif < 2 s) ; deux requêtes de facettes ≈ 85 ms (objectif < 50 ms chacune).
- **Tests** (14 nouveaux dans `taxonomy_repo`, dont un test d'actualisation M3U complète par le moteur) : filtres croisés, sous-thèmes, axe Fournisseur identique à `get_groups`, thème emprunté, surcharges, bouquets masqués, groupes adultes et profil enfant sur chaque axe, valeurs de filtre hostiles, séries, version stockée, annulation, lignes orphelines, SLA.

### Lot 33.3 : interface Live TV

- **Fichiers** : `Shared/CategoryAxisSupport.swift` (axes, libellés français, icônes, drapeaux, tri), `Shared/FacetChipsRow.swift` (chips avec compteurs), `Shared/CategoryNavigation.swift` (étendu : `label`, `icon`, `axisPicker`, valeurs par défaut : Films et Séries inchangés), `Shared/SearchFilterToolbar.swift` (`country`, `theme`, `subtheme`), `Features/LiveTV/LiveTvModel.swift` et `LiveTvView.swift`.
- **Colonne** : sélecteur **Pays · Thèmes · Fournisseur** (choix mémorisé dans `live.categoryAxis`, **Thèmes** par défaut). Pays : drapeau, nom localisé (`Locale`), pays de l'appareil en tête, « International » et « Pays inconnu » en fin de liste. Thèmes : icône SF Symbol, plus peuplés d'abord, « Non classé » en dernier. Fournisseur : liste d'origine, inchangée.
- **Filtre croisé** : rangée de chips de l'autre axe (compteurs du pays ou du thème choisi), rangée de sous-thèmes quand le thème en a, ligne de titre « Sport · France ». Changer d'axe efface la sélection.
- **Compteur « Toutes les chaînes »** : total du catalogue ; sur l'axe Pays, somme des pays que laisse le thème choisi ; sur l'axe Thèmes avec un pays choisi, effectif de ce pays.
- **Recherche** : ignore le pays et le thème choisis, comme elle ignore déjà le groupe du fournisseur, et l'écran le dit. Le filtre « Groupe » de la barre d'outils n'apparaît que sur l'axe Fournisseur.
- **Accessibilité** : chips lues « titre, nombre », état sélectionné annoncé, sélecteur d'axe nommé.
- **Tests** : 16 nouveaux (`CategoryAxisTests.swift`) : drapeaux et noms, tri des pays et des thèmes, libellés des 16 thèmes et des sous-thèmes, titre de sélection, stockage de l'axe, et le modèle Live TV contre un vrai moteur (colonne, chips, compteurs, recherche, axe Fournisseur). Suite Swift complète : 426 tests au vert.
- **Vérification visuelle** : aucun affichage n'est disponible dans l'environnement de développement (la capture d'écran échoue), l'app lancée n'a donc pas pu être vue. La colonne a été **rendue hors écran** (macOS, mode sombre, 900 × 520) pour les axes Thèmes et Pays : mise en page, icônes, drapeaux et chips conformes. **Non compilé pour iOS, tvOS et visionOS** (les plateformes ne sont pas installées sur cette machine ; seul macOS a été compilé et testé). **Non vérifiés à l'écran** : la mise en page étroite (feuille « Catégories » et chips sur iPhone), Apple TV (`focusSection` du sélecteur) et le mode clair.
- **Hors lot** : Films et Séries gardent l'axe Fournisseur jusqu'au lot 33.5 (fait).

### Lot 33.4 : corrections, masquage, profils enfants, recherche, assistant

- **Corrections** (§ 7) : menu d'une chaîne « Changer le pays ou le thème… » et menu d'une catégorie du fournisseur « Classer cette catégorie… » (axe Fournisseur) ouvrent une feuille (pays, 14 thèmes assignables ; `adult` et `other` ne s'assignent pas à la main). Une correction de chaîne l'emporte sur la règle de catégorie ; les deux survivent aux actualisations et aux reclassements. Table `taxonomy_group_overrides` (clé = libellé normalisé). Lecture pour la feuille : `get_item_taxonomy`, `get_taxonomy_override`, `get_taxonomy_group_override`. Réinitialisation : Réglages > Catégories (`reset_taxonomy_overrides`, avec confirmation et compteurs).
- **Masquage** : clic droit sur un pays ou un thème de la colonne (« Masquer ce pays / ce thème »), par profil, stocké dans `hidden_groups` avec les types `country:live` et `theme:live` (donc synchronisé par iCloud comme les bouquets). Un thème masqué cache les chaînes dont c'est le **thème principal**, et disparaît de la liste des thèmes ; un pays masqué cache aussi les chaînes sans libellé de fournisseur. Réaffichage : éditeur de bouquets, section « Pays et thèmes masqués ». Appliqué aux listes, aux séries, aux facettes et aux recommandations.
- **Profils enfants** : l'écran des catégories autorisées propose les **thèmes** (Chaînes seulement) ; autoriser un thème autorise ses chaînes, sauf celles des catégories adultes. `allow_group` refuse `theme:*` + `adult`.
- **Recherche** : l'index plein texte couvre les noms du pays et des thèmes (`tx_terms`, FR et EN, `adult` exclu) : « sport italie », « infos france ». Un index sans cette colonne est reconstruit une fois à l'ouverture (testé sur une base à l'ancienne définition) ; `TAXONOMY_VERSION` passe à 2 pour remplir les mots des bases existantes.
- **Assistant ⌘K** : un thème et un pays dans la demande (« le sport en France », « une chaîne d'info en anglais », la langue donnant ses pays) interrogent le classement et passent juste après ce que le moteur local a déjà trouvé. Un thème seul ou un pays seul n'ajoute rien (la recherche par mots profite déjà des nouveaux mots indexés).
- **Vitesse** : les contrôles de masquage sont gardés par un test évalué une fois (le profil masque-t-il un pays ou un thème ?) : listes à 88 ms pour deux requêtes sur 50 000 éléments, comme avant le lot ; reclassement 1,2 s.
- **Tests** : Rust, 49 tests de taxonomie (dont masquage par profil, thèmes enfants, règles de catégorie, réinitialisation, recherche, mise à niveau de l'index, assistant) ; Swift, 9 nouveaux (`CategoryCorrectionTests.swift`), 435 au total au vert. Feuille de correction rendue hors écran (macOS, mode sombre) : conforme.
- **Non fait** : ordre personnalisé des pays par glisser-déposer (l'ordre reste : pays de l'appareil, puis alphabétique) ; recherche par pays et thème des séries (elle cherche encore dans le titre et le libellé) ; thèmes des enfants pour films et séries (lot 33.5). **Non vérifié à l'écran** : menus contextuels de la colonne et des chaînes, feuille sur iPhone, Réglages ; **non compilé** pour iOS, tvOS et visionOS.

### Lot 33.5 : genres des films et séries, indice du guide

- **Genres** : pour les éléments `movie`, `episode` et `series` seulement, un second dictionnaire (`GENRE_KEYWORDS`, 8 langues) donne un **genre en sous-thème** : action, aventure, comédie, policier, drame, famille, fantastique, horreur, mystère, romance, science-fiction, thriller, guerre, western, biographie (+ `classic` et `animation` déjà existants). Le thème reste `movies` (ou `series` pour une série ou un épisode) : « Cinéma » se déplie en genres comme « Sport » en sous-thèmes. `Documentary`, `Kids`, `Music`, `News`… restent des thèmes à part entière. Un film n'a jamais le thème `series` ni l'inverse : `Movies;Series` sur un épisode donne `series`. Les chaînes Live ne lisent pas les genres (`Action` reste `other`, `Comedy` reste `entertainment/comedy`).
  - Un genre précis remplace un qualificatif (`Classic;Comedy;Movies` → `movies/comedy`). Un élément n'a qu'un genre par thème (clé primaire `(item_id, theme)`) : `Action;Comedy` donne le premier.
  - Les mots d'une expression reconnue ne sont plus relus seuls (`science fiction` n'est pas `science`).
- **Séries** : colonne `series_catalog.tx_subtheme` (genre du thème principal) ; `TaxonomyFilter.subtheme` s'applique aux séries ; les facettes de séries déplient les genres comme celles des films. `TAXONOMY_VERSION` passe à **3** (reclassement unique au prochain lancement).
- **Indice du guide (XMLTV `<category>`)** : une chaîne encore `other` qui a un `tvg-id` prend le thème sur lequel **la majorité** de ses programmes s'accordent (au moins 3 programmes, au moins la moitié de ceux qui nomment un thème). Dictionnaire `EPG_KEYWORDS` (« Informazione », « Cartoni Animati », « Nieuws », « Sarjat »… et genres de films → `movies`). Jamais `adult`, `general` ni `other` ; ne touche ni une correction de l'utilisateur ni un thème déjà trouvé ; provenance `epg`. Lancé à la fin de l'import du guide et après chaque classement (donc conservé par un reclassement complet) ; les mots du thème sont ajoutés à la recherche, y compris pour le thème emprunté par `tvg-id` (oubli du lot 33.2 corrigé).
- **Interface** : Films et Séries ont le sélecteur **Pays · Thèmes · Fournisseur** (par défaut Thèmes, choix mémorisé dans `movies.categoryAxis` et `series.categoryAxis`), les chips de l'autre axe, la rangée de genres et le titre « Cinéma (Action) · France ». « Classer cette catégorie… » (axe Fournisseur) y est disponible. Le moteur de colonne est celui du Live TV.
- **Mesures sur une copie de la base locale** (12 229 chaînes, 889 films, 540 épisodes, 540 séries) : reclassement complet 2,7 s en `debug` ; **67 chaînes** classées par le guide (le guide de cette base ne donne une catégorie qu'à 82 chaînes) ; thème `other` : 2 866 chaînes soit **23,4 %** (24,0 % au lot 33.2). **L'objectif ≤ 15 % n'est pas atteint** sur cette base : 1 536 chaînes `Undefined` et la plupart des chaînes rangées sous un simple nom de pays n'ont ni thème, ni `tvg-id` partagé, ni guide qui nomme un thème. Les films et séries de cette base ont des libellés iptv-org (`Movies`, `Series`, `Comedy;Series`) : seul `comedy` y apparaît (12 séries) ; les genres se verront avec un fournisseur Xtream qui nomme ses catégories (`FR| ACTION`).
- **Tests** : Rust, 53 tests de taxonomie (genres des films et séries, remplacement du qualificatif, expression `science fiction`, majorité du guide, seuils, correction prioritaire, conservation par un reclassement complet, recherche, facettes et filtre de genre des films et séries, valeur de genre hostile) ; fichier de référence relu : 11 libellés VOD changés (`Comedy;Movies` → `movies/comedy`, `FR| FILMS ACTION` → `movies/action`…) ; Swift, 3 nouveaux (`VodGenreTests`), **438 tests au vert**. `cargo clippy --workspace --all-targets` sans avertissement. Le test `test_50k_streaming_m3u_ingestion_sla` échoue toujours quand toute la suite tourne en parallèle et passe seul.
- **Non fait** : thèmes des profils enfants pour films et séries (le moteur sait filtrer `theme:movie` / `theme:series`, mais l'écran des catégories autorisées ne les propose que pour les chaînes) ; masquer un pays ou un genre en Films et Séries (pas d'écran pour les réafficher) ; recherche des séries par pays et thème ; plusieurs genres par film ; ordre personnalisé des pays. **Non vérifié à l'écran** : Films et Séries avec l'axe Thèmes et la rangée de genres (aucun affichage disponible dans l'environnement) ; **non compilé** pour iOS, tvOS et visionOS.
