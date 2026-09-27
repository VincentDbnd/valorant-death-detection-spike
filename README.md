# Spike : détecter la mort du joueur dans Valorant par capture d'écran

> **Statut : spike terminé, non poursuivi.** Ce dépôt est un prototype
> d'exploration. Il est publié pour la démarche et les mesures, pas comme un
> outil prêt à l'emploi.

## Objectif

Savoir en temps réel si le joueur est **vivant** ou **mort** dans Valorant,
**uniquement à partir de ce qui est affiché à l'écran**. L'outil ne lit pas la
mémoire du jeu, n'injecte rien et n'interagit pas avec le client : il capture
l'image comme le ferait un logiciel de streaming.

L'idée : quand le joueur est mort, le jeu passe en mode spectateur et affiche
un bandeau caractéristique. Si on repère ce bandeau dans une zone fixe de
l'écran, on sait que le joueur est mort.

## Démarche

Le spike a avancé par petites étapes, chacune validée par des mesures réelles
avant de passer à la suivante :

1. **Capture.** Peut-on capturer une région de l'écran pendant que le jeu
   tourne en plein écran ? Utilisation de Windows.Graphics.Capture (WGC) via
   [`windows-capture`](https://crates.io/crates/windows-capture), avec un test
   « l'image est-elle noire ? ».
2. **Mesure.** Un template (image de référence du bandeau) est comparé à la
   région capturée par **corrélation croisée normalisée (NCC)**, implémentée
   à la main (images intégrales et produit scalaire vectorisable). Le score
   vaut environ 1 quand le bandeau est présent et reste autour de 0 sinon.
   Les pourcentages de la région sont appliqués à la zone de jeu 16:9 pour
   gérer les écrans ultra-larges.
3. **Instrumentation.** Chaque mesure est enregistrée en CSV. Les images
   autour des chutes de score peuvent être sauvegardées pour comprendre ce qui
   se passe à l'écran (`--dump-transitions`).
4. **Robustesse de la source.** Capture d'une fenêtre précise plutôt que du
   moniteur (`--game-window`), et deux colonnes de diagnostic :
   `foreground` (le jeu est-il la fenêtre active ?) et `capture_ok` (la frame
   est-elle noire ou figée ?).
5. **Décision.** Une machine à états transforme le flux de scores en un état
   `Alive` / `Dead` stable, avec une hystérésis asymétrique. Elle est rejouée
   sur les CSV enregistrés pour la calibrer (`replay`, `sweep`).

## Ce que les mesures ont montré

- **Le signal est net.** Le score est bimodal : autour de 1.0 quand le
  bandeau est affiché, entre -0.2 et 0.2 sinon.
- **Décrochages d'une frame.** En mode spectateur, chaque changement de
  joueur observé fait chuter le score sur une seule frame. D'où
  l'hystérésis.
- **Alt-tab.** Hors du jeu, une capture du moniteur voit le bureau et conclut
  à tort que le joueur est vivant. La capture par fenêtre et la colonne
  `foreground` traitent ce cas.
- **Cas ambigus.** Certaines séquences donnent des scores intermédiaires
  (0.3 à 0.9) pendant une à deux secondes, ce qui provoque des bascules
  parasites selon les paramètres choisis.

## Pourquoi le spike s'arrête là

La détection fonctionne dans le cas nominal, mais chaque nouvelle session
révèle une situation particulière (alt-tab, écran noir, frame figée, scores
intermédiaires…). Le nombre de contraintes et de paramètres à régler au cas
par cas rend l'approche trop fragile pour être fiable sans un gros travail
supplémentaire.

## Architecture

```
src/
├── main.rs      Point d'entrée et sous-commandes (clap)
├── capture.rs   Capture WGC, géométrie letterbox, NCC, snap/extract/watch
│                (compilé sous Windows uniquement)
├── detector.rs  Machine à états Alive/Dead, sans dépendance, testée
└── replay.rs    Rejeu de CSV et balayage de paramètres (toutes plateformes)
```

La machine à états ne dépend ni de la capture ni du système : c'est une
fonction du flux de mesures vers un état, ce qui permet de la tester et de la
rejouer hors de Windows.

## Compilation

Rust édition 2024.

```sh
# Windows, depuis Linux/WSL (toutes les sous-commandes)
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu

# Linux natif (replay et sweep uniquement)
cargo build --release
cargo test
```

## Utilisation

Sous Windows (capture) :

```sh
# Capture une région en PNG et indique si l'image est noire
capture snap --game-window valorant --x 40 --y 5 --w 20 --h 10 --out region.png

# Découpe le template de référence (depuis l'écran ou un PNG existant)
capture extract --game-window valorant --x 40 --y 5 --w 20 --h 10 --out template.png

# Mesure en continu et enregistre un CSV
capture watch --game-window valorant --template template.png \
    --x 38 --y 3 --w 24 --h 14 --hz 10 --log session.csv
```

Sur n'importe quelle plateforme (analyse) :

```sh
# Rejoue une ou plusieurs sessions dans la machine à états
capture replay --csv session1.csv session2.csv --frames-to-dead 5 --frames-to-alive 2

# Nombre de transitions pour chaque combinaison de paramètres
capture sweep --csv session1.csv session2.csv
```

Les coordonnées sont des pourcentages de la zone de jeu. Les valeurs
ci-dessus ne sont que des exemples.

## Avertissement

Projet personnel sans lien avec Riot Games. Valorant est une marque de Riot
Games, Inc.

## Licence

[MIT](LICENSE)
