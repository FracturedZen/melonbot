# MelonBot

Headless Minecraft bots in Rust, built on [azalea](https://github.com/azalea-rs/azalea) (MC 26.2).
No game client needed. Each bot signs in with its own Microsoft account. It is driven from a local
web control panel.

Forked from [EnderKill98/stasis-bot](https://github.com/EnderKill98/stasis-bot), the azalea-based
stasis bot by EnderKill98 (MIT). See [Credits](#credits).

What is in it:

- **The melon farmer** (`start-melon-farmer.cmd`, `melonbot run <folder>`): a port of the MelonHack
  Java client's melon routine. It flies to its farm, harvests with a Silk Touch axe, keeps itself fed
  and tooled from signed chests, and prints `.litematic` / `.schem` builds out of melons.
- **The botswarm** (`start-botswarm.cmd`, `melonbot swarm`): any number of accounts in one process,
  with formations, follow-me, a box nuker, a printer and combat. It can take orders from a
  controller (a Java client) as well as from its panel.
- **The perimeter bots** (`start-perimeter.cmd`, `melonbot perimeter`): clear a box from build height
  down to bedrock, sealing water and lava as they go.
- **The seller and the buyer** (`start-seller.cmd`, `start-buyer.cmd`): trade with the villagers in a
  pen, selling goods for emeralds and spending emeralds on a buy list.

Use them only on servers that allow bots.

## Build (Windows)

- Rust nightly (`rust-toolchain.toml` pins it) and the Visual Studio 2022 C++ Build Tools.
- `cargo build --release`. The launchers run `target\release\melonbot.exe`.

## Accounts, folders and the first sign-in

Every bot of its own has a folder `bots/<folder>/` holding its `config.json`, its Microsoft login
cache (`azalea-auth.json`) and its logs. The swarm and perimeter accounts live in `swarm/bots/<name>/`.
`bots/`, `swarm/` and `perimeter/` are git-ignored. They hold logins and webhooks, so never commit
or share them.

The first start of a bot prints a microsoft.com/link address and a code. Sign in with the account
the bot is to play as. The login is cached from then on. Leave `"account"` empty in config.json and
the first sign-in fills it in. Once set, a login that comes back as anyone else is refused.

Never run a bot and a Java client on the same account at once.

## Settings (config.json)

The main fields:

- `server`: the server to join (`host` or `host:port`).
- `queue_command`, `queue_trigger`: for a server with a queue/lobby. When chat matches the trigger
  (a regex), the bot sends the command every minute until it is back. Leave `queue_command` empty
  for none.
- `discord_webhook`, `discord_name`: optional Discord posts. The webhook is never printed.
- `player_guard`, `friends`: with the guard on, any player in view who is not a friend or one of
  your own accounts makes the bot log out and stay out (exit code 3) until relaunched.
- `board_server`: the server whose melon statistics go to a MelonBoard leaderboard. Its URL goes in
  `bots/<folder>/melonboard.json` (`endpoint`). Empty means no reporting.

`melonbot import "<Java instance folder>"` writes a melon farmer's config.json from a MelonHack Java
instance: the account guard, Discord, queue settings, farm plot, base and signed chests.

## The melon farmer

```
melonbot import "<path to the Java instance>"
start-melon-farmer.cmd <folder>
```

Its control panel is http://127.0.0.1:6971/ (`open-panel.cmd`).

## The botswarm

Its panel is http://127.0.0.1:7000/ (`open-swarm-panel.cmd`). Add accounts there. Each gets a
microsoft.com/link code on the page. From the panel you set orders, formations, the nuker box, the
printer, the ender chest link and combat. Settings are in `swarm/swarm.json`.

## Perimeter bots

`start-perimeter.cmd` clears a box from build height down to bedrock. Its panel is
http://127.0.0.1:7100/ (`open-perimeter-panel.cmd`). There you tick the accounts it opens, add
accounts, set the box, run or pause, and watch a map of the box. A new box starts paused. The
accounts are the botswarm's (`swarm/bots/`), and an account runs in one of the two at a time.

- The box is cut in 8x8 cells, a band at a time from the top. No cell goes deeper than a neighbour
  that is higher, or more than two bands below the highest ground left. The box is scouted first,
  and air is skipped without a visit.
- A block with water or lava beside or above it is never broken until that fluid has a block in it.
  Those blocks are dug out later inside the box. Outside the box they stay, as the edge wall.
- The fill is what the bots dig (cobblestone, dirt, deepslate...), held in the offhand. Everything
  else they dig is thrown out when they need room.
- Grass, ferns, leaf litter and natural leaves are left. They drop or decay once what holds them is
  gone.
- A block the server keeps putting back (WorldGuard, a world border) is left standing. So is a whole
  side of the box once three of its blocks were.
- Log each account in by the chests signed `tools`, `repair` and `food`, and it records them there.
  Worn tools go into Repair. A pickaxe, shovel and axe come out of Tools, and food out of Food.

Files: `perimeter/job.json` (the box and settings), `progress.json` (every cell's floor),
`accounts.json`, `bases.json`, and logs in `perimeter/logs/` and `perimeter/bots/<name>/debug/`.

## Seller and buyer (the villager pen)

Two bots share a pen of villagers, marked out by corner signs reading `1`, `2`, ... (or `bounds` in
config.json). Each finds its chests by the signs on them, and only moves inside the box.

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
  buyer never touches the `emeralds` chest. Edit the list on the **Shop** tab.

Both eat from the chest signed `food`, and respawn on the bed set inside the box.

## Credits

- [EnderKill98/stasis-bot](https://github.com/EnderKill98/stasis-bot) by EnderKill98, the project
  this one was forked from (MIT; its copyright notice is kept in [LICENSE](LICENSE)).
- [azalea](https://github.com/azalea-rs/azalea), the Rust Minecraft bot library everything here is
  built on (MIT).

## License

MIT - see [LICENSE](LICENSE).
