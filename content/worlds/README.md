# World packages

Included scenarios are `castle`, `siege` and `succession`. Each package contains:

- `world.json`: permanent IDs, connections, custody, equipment, needs, skills,
  relationships and canonical starting fields.
- `locales/en.json` and `locales/pl.json`: complete names, descriptions, private
  biographies, personality, goals, knowledge and objectives.
- `prompts/npc.<language>.md` and `prompts/listener.<language>.md`: shared behavior
  contracts and atmosphere. Shared prompts never carry one actor's secrets.
- Optional `legacy-text.json`: authored source text to stable locale-field paths.

Only the engine validates and changes physical facts. Edit fiction in locales;
writing that someone owns more food does not grant it. Blueprint changes apply
to a new castle; saves retain their physical state.

Actor names use seven explicit forms: `nom`, `gen`, `dat`, `acc`, `inst`, `loc`,
`voc`. The nominative must match the displayed name. Authored references such as
`{actor:102:gen}` resolve by ID. Aliases reserve old NPC names and make legacy
memories comprehensible; they never replace substrings in player dialogue.
Mechanical roles remain canonical English, while displayed roles are localized.

All locales must cover exactly the blueprint's entities. Validation checks IDs,
door endpoints, item custody, actor knowledge references, name collisions and
all authored name references. The dungeon begins with prisoner 101 and exactly
one guard, 102; the king remains 103. No locale file may be executable code.

Install a package directory under
`$XDG_DATA_HOME/below-the-crown/scenarios/<scenario-id>/`; its blueprint must
contain the matching `scenarioId`. `BELOW_THE_CROWN_SCENARIOS_DIR` selects another
installation directory. Explicit package paths are accepted by the content API.
Use `./package` or an absolute path, and give external packages a distinct ID.
Builtin IDs always select their embedded package. Validate before replacing a
live castle; an invalid package leaves it intact.

Portraits are shared cosmetic artwork in `../portraits`, not duplicated per world.
New prisoners inherit the selected world's prisoner kit and limited knowledge,
choose their name, then describe their story through speech.
