# MelonBot

Headless Minecraft bots in Rust, built on [azalea](https://github.com/azalea-rs/azalea) (MC 26.2).
No game client needed. Each bot signs in with its own Microsoft account. It is driven from a local
web control panel.

Forked from [EnderKill98/stasis-bot](https://github.com/EnderKill98/stasis-bot), the azalea-based
stasis bot by EnderKill98 (MIT). See [Credits](#credits).

What is in it:

- **[The melon farmer](#the-melon-farmer)** (`start-melon-farmer.cmd`): flies to its melon farm,
  harvests with a Silk Touch axe, keeps itself fed and tooled from signed chests, and prints
  `.litematic` / `.schem` builds out of melons.
- **[The botswarm](#the-botswarm)** (`start-botswarm.cmd`): up to 10 accounts in one process, with
  formations, follow-me, a box nuker, a printer and combat.
- **[The perimeter bots](#the-perimeter-bots)** (`start-perimeter.cmd`): clear a box from build
  height down to bedrock, sealing water and lava as they go.
- **[The seller and the buyer](#the-seller-and-the-buyer)** (`start-seller.cmd`,
  `start-buyer.cmd`): trade with the villagers in a pen, selling goods for emeralds and spending
  emeralds on a buy list.
- **[The mapart bot](#the-mapart-bot)** (`start-mapart.cmd`): builds carpet map art on a platform,
  one map at a time. It lays the carpets, maps them, locks and names the map, stores it and washes
  the platform clear. Maps come in through a folder, or through Discord if you set that up.

Use them only on servers that allow bots.

## Getting started

### Build (Windows)

1. Install Rust with [rustup](https://rustup.rs). The repo's `rust-toolchain.toml` picks the
   nightly it needs, and rustup fetches it on the first build.
2. Install the Visual Studio 2022 Build Tools with **Desktop development with C++**.
3. Get the code and build it:
   ```
   git clone https://github.com/FracturedZen/melonbot
   cd melonbot
   cargo build --release
   ```

The launchers (`start-*.cmd`) run `target\release\melonbot.exe` from the repo folder. Double-click
one, or run it from a terminal in that folder. A launcher restarts its bot 30 s after it stops,
except after a deliberate logout (see [Troubleshooting](#troubleshooting)). Close its window to
stop it.

### Folders and the first sign-in

Each bot of its own (farmer, seller, buyer, mapart) has a folder `bots/<folder>/`. It holds the
bot's `config.json`, its optional `modules.json`, its Microsoft login cache (`azalea-auth.json`)
and its logs. The botswarm and the perimeter bots keep their accounts in `swarm/bots/<name>/`.

`bots/`, `swarm/` and `perimeter/` are git-ignored. They hold logins, webhooks and tokens, so never
commit or share them.

The first start of a bot prints a microsoft.com/link address and a code in its window. Open the
address, enter the code, and sign in with the account the bot is to play as. The login is cached
from then on. Leave `"account"` empty in config.json and the first sign-in fills it in. Once set, a
login that comes back as anyone else is refused.

Never run a bot and a game client on the same account at once.

### config.json

Every bot of its own needs `bots/<folder>/config.json`. A minimal one:

```json
{
  "server": "play.example.net",
  "job": "mapart"
}
```

The fields any bot can have:

| Field | What it does |
| --- | --- |
| `server` | The server to join (`host` or `host:port`). |
| `account` | The account it plays as. Leave it empty; the first sign-in fills it in. |
| `job` | `"seller"`, `"buyer"` or `"mapart"`. Empty (or left out) is the melon farmer. |
| `player_guard` | On by default. See [The player guard](#the-player-guard). |
| `friends` | Players the guard never logs out for. |
| `queue_command`, `queue_trigger` | For a server with a queue or lobby. When chat matches the trigger (a regex), the bot sends the command every minute until it is back. Empty command: off. |
| `discord_webhook`, `discord_name` | Optional Discord posts through a webhook. The webhook is never printed. |
| `board_server` | The server whose melon statistics go to a MelonBoard leaderboard. Its URL goes in `bots/<folder>/melonboard.json` (`endpoint`). Empty: no reporting. |

Each bot's own fields are in its section below.

### modules.json

Optional, in `bots/<folder>/modules.json`, in the shape of the MelonHack client's
`standalone.json`: each module's name, whether it is on, and its settings.

```json
{
  "control-panel": { "enabled": true, "settings": { "port": 6972 } },
  "auto-eat": { "enabled": true, "settings": {} },
  "kill-aura": { "enabled": true, "settings": { "targets": ["minecraft:zombie", "minecraft:skeleton"], "range": 4.5, "auto-weapon": true } }
}
```

- `control-panel`: the bot's own web page, on `http://127.0.0.1:<port>/`. **Without it the bot has
  no panel.** Give each bot its own port (the melon farmer's is 6971). If that port is taken, the
  next free one up to 20 higher is used, and the log says which.
- `auto-eat`: eats from what it carries. The melon farmer only eats with this on. The seller,
  buyer and mapart bot eat anyway.
- `kill-aura`: hits the listed mobs in range. With a mace in the hotbar, mobs farther out are
  struck from above. It never strikes past the world border.
- `ai-pilot`: the melon farmer's routine (see [The melon farmer](#the-melon-farmer)).

### The player guard

`player_guard` is on unless you set `"player_guard": false`. With it on, any other player who comes
into view makes the bot log out and stay out until you start it again.

Players who never count:

- your own accounts: every account the bots sign in with is written to
  `%USERPROFILE%\.melonhack\own-accounts.txt`. Add your main account there, one name a line, so
  visiting your own bot does not log it out;
- the names in `friends`.

### Control panels

| Panel | Address | Opens with |
| --- | --- | --- |
| A bot's own | `http://127.0.0.1:<port>/` from its modules.json (melon farmer: 6971) | `open-panel.cmd` (6971) |
| The botswarm | http://127.0.0.1:7000/ | `open-swarm-panel.cmd` |
| The perimeter bots | http://127.0.0.1:7100/ | `open-perimeter-panel.cmd` |

The panels only answer on this PC (127.0.0.1).

## The melon farmer

It runs in a loop:

1. Food: from the chest signed `food`, or melon slices when that is empty.
2. A usable Silk Touch axe: worn ones go into the chest signed `repair`, and the best one comes
   out of the chest signed `tools`.
3. Melon blocks from the farm, until its bags are full.
4. The next build on its list, printed out of those melons.

Once the list is built, it lays a melon floor on open water around its base, or a ceiling art if
you set one.

### Setting it up

1. Near its base, put chests signed `tools` (Silk Touch axes), `repair` (empty, for worn axes) and
   `food`.
2. Write `bots/<folder>/config.json`:
   ```json
   {
     "server": "play.example.net",
     "base": { "x": 100, "y": 64, "z": 100 },
     "farm": { "min": { "x": 120, "y": 60, "z": 120 }, "max": { "x": 180, "y": 66, "z": 180 } },
     "schematics": "C:\\Users\\you\\schematics"
   }
   ```
   - `base`: where it lives; the signed chests are found around it.
   - `farm`: two corners of the melon farm.
   - `schematics`: the folder its build files are read from.
3. Write `bots/<folder>/modules.json`:
   ```json
   {
     "control-panel": { "enabled": true, "settings": { "port": 6971 } },
     "auto-eat": { "enabled": true, "settings": {} },
     "ai-pilot": { "settings": {
       "routine": "Melon",
       "auto-continue": true,
       "melon-builds": ["Castle.litematic", "Tower.schem"],
       "melon-floor-y": 64
     } }
   }
   ```
   - `routine` `"Melon"` with `auto-continue` on starts the routine after every join.
   - `melon-builds`: the files in the schematics folder, built in that order. Each goes at the
     nearest open spot at least `melon-home-clear` blocks (200) from the base and the farm, and
     `melon-gap` blocks (20) clear of anything built.
   - After the list: `melon-floor` (default `MELON FLOOR.litematic`) is laid on open water at
     `melon-floor-y`, spiralling out from the base. Set `melon-ceiling` to a file to lay that art
     instead.
4. Run `start-melon-farmer.cmd <folder>`. Without a folder it uses `bots/MelonFarmer/`.

Its panel is http://127.0.0.1:6971/ (`open-panel.cmd`). There you can start and stop the routine,
or disconnect the bot. Its progress (builds done, sites, melons mined) is kept in
`bots/<folder>/melon-routine.json`.

`melonbot schem <file>` prints what it would build from a schematic.

If you run the MelonHack Java client, `melonbot import "<instance folder>"` writes the config.json
and modules.json from that instance: its account, Discord, queue settings, farm plot, base and
chests.

## The botswarm

`start-botswarm.cmd` runs every swarm account in one process. Its panel is
http://127.0.0.1:7000/ (`open-swarm-panel.cmd`). The first start writes `swarm/swarm.json`: set its
`server` to yours, then start it again.

- **Accounts**: press **+ Add account** on the panel. It shows a microsoft.com/link code. Sign in
  with the account to add (not one with a bot of its own, nor the one you play on), and it joins
  the swarm. Up to 10 are online at once (`max_bots`).
- **Orders**, for the whole swarm or one bot: hold, go to a point (several bots spread around it),
  come here, stay near a player, or take a formation.
- **Formations**: circle, square, triangle, star, heart, wave, sphere, helix, cube, line, scatter
  or random, around a centre. A shape can flow or turn at a set speed.
- **The nuker**: clear a box set on the panel. Each bot takes the nearest free 8x8 column, so no
  two dig the same place.
- **The printer**: the swarm prints, between them, what you have placed in Litematica on a Java
  client running the MelonHack controller mod. The schematic file comes from `swarm/schematics/`,
  else from where Litematica found it.
- **Combat**: with Defend on, the bots fight hostile mobs and any player who hits one of them.
  Friends and your own accounts are never attacked. You can also set an attack target.
- **The ender chest link**: each bot keeps its ender chest open from a distance, and takes tools,
  weapons, food and gear from it as needed.
- **Supplies**: food from a chest signed `food`, tools from a chest signed `tools`, and what the
  nuker collects into a chest signed `storage`.

Settings are in `swarm/swarm.json` (server, panel port, speed, friends, signs) and
`swarm/modules.json` (module settings shared by every swarm bot). The swarm can also take orders
from a MelonHack Java client running the controller mod (its Come Here, Stay Near, nuker and
printer). Everything else works from the panel alone.

## The perimeter bots

`start-perimeter.cmd` clears a box from build height down to bedrock. Its panel is
http://127.0.0.1:7100/ (`open-perimeter-panel.cmd`). The accounts are the botswarm's
(`swarm/bots/`), added on either panel. An account runs in one of the two at a time.

### Setting it up

1. Near where they will work, put chests signed `tools` (pickaxes, shovels and axes), `repair`
   (empty, for worn tools) and `food`.
2. Start `start-perimeter.cmd` once. It writes `perimeter/perimeter.json`: set its `server` to
   yours, and start it again. Then open its panel.
3. Tick the accounts it should run (add them there if they are new).
4. Log each account in by the signed chests once. It records them as its base.
5. Set the box's corners, then press Run. A new box starts paused.

### How it digs

- The box is cut in 8x8 cells, a band at a time from the top. No cell goes deeper than a neighbour
  that is higher, or more than two bands below the highest ground left. The box is scouted first,
  and air is skipped without a visit.
- A block with water or lava beside or above it is never broken until that fluid has a block in it.
  Those blocks are dug out later inside the box. Outside the box they stay, as the edge wall.
- The fill is what the bots dig (cobblestone, dirt, deepslate...). Everything else they dig is
  thrown out when they need room. A bot that runs out mines more close by, at any depth inside the
  box: stone, dirt and the like that it can reach from open air, with no water beside it.
- Grass, ferns, leaf litter and natural leaves are left. They drop or decay once what holds them is
  gone.
- A block the server keeps putting back (WorldGuard, a world border) is left standing. So is a whole
  side of the box once three of its blocks were. A block holding back water or lava there, which no
  block can be put into, stays too, as a wall.
- Worn tools go into `repair`. A pickaxe, shovel and axe come out of `tools`, and food out of
  `food`.
- A bot that is being hurt fast logs out and stays out (its reason shows on the panel). The totem
  stays in its offhand.

Files: `perimeter/job.json` (the box and settings), `progress.json` (every cell's floor),
`accounts.json`, `bases.json`, and logs in `perimeter/logs/` and `perimeter/bots/<name>/debug/`.

## The seller and the buyer

Two bots share a pen of villagers, marked out by corner signs reading `1`, `2`, ... (or `bounds` in
config.json). Each finds its chests by the signs on them, and only moves inside the box. Both eat
from the chest signed `food`, and respawn on the bed set inside the box.

- **Seller** (`start-seller.cmd`, `"job": "seller"`): sells the `sell` list for emeralds. Each good
  comes from the chests signed with its name (an ingot's with the metal: `iron`, `gold`; `sticks`
  for sticks). It is sold to the villagers whose job buys it, such as sticks to fletchers and iron
  to armorers, toolsmiths and weaponsmiths. The emeralds go into the chest signed `emeralds`. It
  buys nothing. Edit the list on the control panel's **Sell** tab.
- **Buyer** (`start-buyer.cmd`, `bots/Buyer/`, `"job": "buyer"`): takes emeralds from the chest
  signed `spend`, either loose or a stack of emerald blocks at a time. It crafts the blocks in its
  own inventory into 9 stacks of emeralds, which needs 10 free slots. It spends the emeralds on the
  `buy` list: blocks, and enchanted books paid with a book from the chest signed `books`. It banks
  each kind in the chest signed with its name. The seller never banks into a `spend` chest, and the
  buyer never touches the `emeralds` chest. Edit the list on the **Shop** tab, and turn spending on
  there (`spend_emeralds`).

### Setting them up

1. Make `bots/<folder>/config.json` for each, with `"job": "seller"` or `"job": "buyer"` and the
   server.
2. Add a `modules.json` with `control-panel` on (a different port each) to get the Sell and Shop
   tabs.
3. The seller's folder is its account name: open `start-seller.cmd` and set `ACCOUNT` to it. The
   buyer's folder is `bots/Buyer/`.
4. Start them, and sign in on the first start.

## The mapart bot

`start-mapart.cmd` runs the bot in `bots/Mapart/` (its config.json has `"job": "mapart"`). For each
map it:

1. lays the carpets on the platform, one colour at a time;
2. maps it with an empty map;
3. locks the map at the cartography table with a glass pane;
4. names it on the anvil;
5. puts it in the `finished` chest;
6. presses the water button to wash the platform clear for the next one.

### Sending it maps

- **The inbox folder** (no setup): drop a `.litematic`, `.schem` or `.nbt` into
  `bots/Mapart/inbox/`. Within a few seconds it is checked and queued under its file's name:
  `Castle.nbt` is built and named `1X1 "Castle"`. A file that fails the check goes to
  `inbox/refused/`, with the reason in a `.txt` beside it.
- **Discord** (optional): anyone with a chosen role drops the files in one channel instead. The bot
  answers each one with a **Name it** button. `queue`, `cancel <n>` and `help` work there too. See
  [Discord for the mapart bot](#discord-for-the-mapart-bot).

The art must be flat and made of carpet only, in whole maps. A 1x1 is 128x128. A 2x2 is built as
its four maps, one after another, named `2X2 "Castle" c1r1`, `c2r1` and so on. Exports from
[mapartcraft](https://rebane2001.com/mapartcraft/) work as they are; their cobblestone support line
is left out.

### The site

The bot finds its site by itself from where it is. Build these:

- **The platform**: a flat floor of any block that covers exactly one map's area. That is 128x128,
  with its north-west corner at an x and a z that are each 64 more than a multiple of 128
  (..., -64, 64, 192, ...). The carpets go on top of it. A row of blocks around it at carpet height
  gives the map's top row its shading.
- **The wash**: water let out over the platform by a button (dispensers in the floor work). Put a
  sign reading `clear` within two blocks of the button. The bot presses it, waits, and presses it
  again.
- **Within 32 blocks of the platform**:
  - Chests of carpet, each signed with its colour alone (`RED`, `LIGHT BLUE`, `GRAY`, `LIME`...).
    Several chests of one colour are fine; it takes from the nearest.
  - Chests signed `maps` (empty maps), `glass pane`, `XP` (bottles o' enchanting, for the anvil),
    `finished` (where the named maps go) and `food`.
  - A cartography table, an anvil and a bed.

Washed-off carpets lie on the platform until they despawn. The bot picks some up as it works and
throws the ones it does not need off the platform. A missing chest or block is named in its log,
and the bot waits until it is there.

### Setting it up

1. Make `bots/Mapart/config.json`:
   ```json
   {
     "server": "play.example.net",
     "job": "mapart"
   }
   ```
   If other players come by the site, add them to `friends` or set `"player_guard": false` (see
   [The player guard](#the-player-guard)).
2. For its control panel, add `bots/Mapart/modules.json` with `control-panel` on (see
   [modules.json](#modulesjson)).
3. Run `start-mapart.cmd`. The first start signs in (a microsoft.com/link code).
4. Log the bot out at its site, by its bed. It looks for the platform in the maps around where it
   is.
5. Drop a map in the inbox.

Its log is in `bots/Mapart/logs/`. The queue is kept in `bots/Mapart/queue.json` and the files in
`bots/Mapart/maps/`, so a restart loses nothing.

### Discord for the mapart bot

Discord is optional. Without it, the inbox is the only way in.

1. At discord.com/developers, make an application with a bot. Turn on its **Message Content**
   intent, and invite it to your Discord server with permission to read and send messages in the
   channel.
2. Put its token in `bots/Mapart/discord-token.txt`, alone in that file. Never commit or share it.
3. In config.json, set `discord_guild_id` (your Discord server), `discord_channel_id` (the channel
   it watches) and `discord_role_id` (the role allowed to send maps). To copy an id, turn on
   Developer Mode in Discord and right-click the server, channel or role. `discord_channel` and
   `discord_role` are their names, for you.

On Discord, the bot only says whether a map was taken and when one is finished. It never posts a
coordinate.

## Troubleshooting

- **Logs**: a bot of its own logs to `bots/<folder>/logs/` and `bots/<folder>/debug/`, the swarm to
  `swarm/debug/` and `swarm/bots/<name>/debug/`, the perimeter bots to `perimeter/logs/` and
  `perimeter/bots/<name>/debug/`.
- **It logged out and did not come back**: that was on purpose (exit code 3). A player came into
  view with the guard on, it was being hurt fast, or you pressed Disconnect. Start it again.
- **"logging in too fast" or a 503 at sign-in**: several bots joined at once. Start them a little
  apart.
- **No control panel**: a bot of its own only has one with `control-panel` in its modules.json.
- **Two of the same bot**: a launcher refuses to start a bot that is already running in another
  window.

## Credits

- [EnderKill98/stasis-bot](https://github.com/EnderKill98/stasis-bot) by EnderKill98, the project
  this one was forked from (MIT; its copyright notice is kept in [LICENSE](LICENSE)).
- [azalea](https://github.com/azalea-rs/azalea), the Rust Minecraft bot library everything here is
  built on (MIT).

## License

MIT - see [LICENSE](LICENSE).
