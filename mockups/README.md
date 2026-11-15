# Second Site HTML mockups

Ten simple linked screens with illustrative data, built with HTML, CSS, and vanilla JavaScript. No install or build required.

Run from the repository root:

```sh
python3 -m http.server 8090 --bind 0.0.0.0 --directory mockups
```

Open http://localhost:8090. Use the sidebar to switch screens.

## Local listing assets

The listing photos, scan images, and OBJ meshes referenced under `mockups/assets/` are local, git-excluded files and are not included in this checkpoint. A fresh clone needs those assets to reproduce the property visuals. Fonts, map tiles, and three.js also need internet access.

## Three-stage workspace

`workspace.html` (with `workspace.css`, `workspace.js`) is the current design: one page that moves from search, to comparing finalists, to furnishing, with one conversation throughout. Open http://localhost:8090/workspace.html. Add `#searched`, `#compare` or `#furnish` to jump to that point of the scripted demo; `#reset` clears saved state. The version saved before the layout pass is `workspace.before-layout.html`, with its own CSS, JavaScript, and separate local storage. It needs internet for the map tiles, fonts and three.js. The chat is scripted, costs and findings are demo data, and six of the nine listings are hue-shifted stand-ins for the three real ones.

Includes landing, onboarding, overview, property search/comparison, property intelligence, design studio, procurement, communications, opening checklist, and settings. The onboarding profile is saved in browser local storage. Other interactions demonstrate UI state only; no APIs, messages, calls, orders, or real approvals occur. Figures and geometry are illustrative.
