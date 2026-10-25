# Changelog

Static recompilation of The SpongeBob SquarePants Movie (GameCube, GGVE78 rev 1) to Rust.
Newest entries first. There is no git history yet, so this file is the record.

## 2026-10-25

### The exe has the game's SpongeBob as its icon
- The user asked for a picture from the game as the exe's icon. The memory card icon
  (files\SBGCIcon.tpl) was too small at 32x32; the menus' round SpongeBob badge, a 128x128
  CI8 texture in MN\mnui.HIP, is used instead. It is game data, so `crates/tssm/build.rs`
  reads it from the extracted disc at build time (`TSSM_DISC`, set by the builder; the
  workspace's disc otherwise), resizes it to 16 to 256 pixels and writes an icon resource
  (.res) for the linker. Without a disc there is no icon and the build goes on.
- The window and taskbar get the same icon (`Icon::from_resource(1)`).
- Checked: the icon read back from the built exe and from the .res at 32 and 256 pixels
  (screenshots); the game starts as before.

### Return on the settings screen keeps the changes
- The user asked that changing a setting (Rumble) and clicking the on-screen Return saves
  it. The game drops changes not accepted with A, so going back from the settings screen
  with the keyboard and mouse (a click on Return, or Escape) now presses A first, then B
  (`menu::press`; options.rs does the same for its own hint clicks). A reaches the game
  for that moment even while Frame Rate or Field of View has the focus.
- Checked headless: Rumble set to Off by a click, then a click on Return: the screen went
  back to the options list, and Off was kept on going in again; the same with Escape.
  Return with nothing changed goes back as before.

### Enter accepts in menus; Yes on the skills menu's question can be clicked
- Enter is bound to Start, which the settings screen ignores: changes there needed Space
  (A, "Accept changes") before Return (the user's report). In menus, with the keyboard and
  mouse in use, Enter (and the keypad's) now presses A, as Escape presses B. The level
  select (pause) screen is left alone, so Enter still goes back to the game from it.
- The skills menu's "Are you sure you want this upgrade?": no widget takes the pad while
  it asks (the menu's own controller reads it), so the mouse code saw no menu and ignored
  the click on Yes or No, and Enter was Start. The question is now recognised by the
  menu's "Upgrade description" hint (`UPGRADE_HINT`) with nothing holding the focus: a
  click on Yes or No presses its button, and Enter and Escape work as in other menus.
- Tests: `TSSM_UPGRADE_POINTS=5` gives points to spend (the game's `globals` + 0x1EC8);
  `TSSM_MOUSE` takes `enter@N`.
- Checked headless with screenshots: settings, a click on Rumble's Off and Enter, then out
  and back in: Off kept (without Enter it was not, as with Space). Skills: a click on
  Health, a second click asks, a click on Yes: the game's note on the upgrade came up, and
  after it points 5 -> 4 and Health's first bar filled. Not checked: Enter on the pause
  screen (the scripted key is pressed and released at once, which only counts for keys
  this code turns into presses; it takes the same path as before).

### No command prompt window with the game
- Release builds are Windows (GUI) programs, so starting the game no longer opens a
  console beside it (the user asked). Started from a terminal they attach to it and print
  there; redirected output (`2> log.txt`) works as before. Debug (`-Dev`) builds keep the
  console. Checked: the exe's PE subsystem is 2, no console child process, and a redirected
  run logged as usual.

### The builder makes "The SpongeBob Movie Game.exe" next to build.bat
- The user built from the v0.1.0 package, could not find `dist\TSSM.exe`, searched for it,
  and every `TSSM.exe` found closed at once. Windows search starts a program in another
  folder, so the game looked for `disc` and `config` in the wrong place; the other two
  copies were cargo's, in `target`, with no game files beside them at all.
- `tools\build.ps1` now puts `The SpongeBob Movie Game.exe` with `disc`, `config` and
  `saves` in the build.bat folder (`-Out` still picks another). A v0.1.0 build's `dist\disc`
  and `dist\saves` are moved there and `dist\TSSM.exe` is deleted.
- The game (desktop, no disc folder given) uses the files next to its executable when
  there is no `disc` in the folder it was started in; with none there either it shows a
  message saying which program to start, instead of closing silently.
- Checked in the user's Spongebobbb folder: the rebuild moved the disc and made the exe;
  started from `C:\Windows\System32` it ran, and `target\release\tssm.exe` started from
  there showed the message.

## 2026-10-24

### One step from a disc image to the game, and a package to hand out
- `build.bat` (double-click; asks for the image) or `tools\build.ps1 -Iso <image> [-Out dist]
  [-Dev] [-Force] [-Yes]`:
  1. checks for Rust and the Visual Studio C++ build tools with a Windows SDK (the SDK by
     its `kernel32.lib`, since an installer can leave an empty folder). A missing one is
     offered with a Y/N question and, on yes, downloaded and installed: rustup-init from
     rust-lang.org (`-y --profile minimal`), the Build Tools from Microsoft
     (`--passive --wait`, VC tools and the Windows 11 SDK 22621; exit code 3010 accepted).
     `-Yes` answers yes without asking.
  2. checks the image is GGVE78 revision 1 and that `sb04gc_nm.elf` has the SHA-1 the port
     is made for.
  3. extracts the disc into `<Out>\disc`, now with `crates\recomp\src\bin\extract.rs` (a
     Rust version of `tools\extract_disc.py`, so Python is no longer needed; the same 196
     files byte for byte, in 4 seconds).
  4. recompiles into `crates\game\src\gen`, 5. builds the release executable, 6. puts
     `TSSM.exe` and `config\` beside the disc.
  Each step is skipped when already done (the recompilation by a stamp of the ELF's and
  `config\hle.txt`'s hashes); config files already in `<Out>` are kept, except `hle.txt`.
- `build.bat` run on its own (copied out of the folder) now says it needs the rest of the
  package, instead of PowerShell's "argument to -File does not exist".
- `tools\package.ps1` makes `TSSM-Builder.zip` (0.2 MB): `build.bat`, a README, the
  workspace and toolchain files, the crates' sources without `crates\game\src\gen`,
  `tools\build.ps1`, and default settings (frame rate `display`, no field of view file).
  Nothing from the game: no recompiled code, disc files, images, saves or build output.
- Run in this project: 3 minutes 10 seconds in all (release build 2 minutes 45 seconds with
  the dependencies already built). The result started from its folder and ran headless
  into `BB02`.

### Driving levels: the car behaves the same at any frame rate
- Reported: the driving levels feel heavier since the frame rate was unlocked.
- `zCar::CarUpdate` moves the car by velocity × time, but adds some of its forces to the
  velocity once per step without scaling them by the time. `zEntPlayerCarUpdate` already
  splits a long frame into steps of at most 1/60 s (a global set to 60), so the car was
  built for steps of 1/60 s; at 240 fps each step was 1/240 s and those forces acted four
  times as often.
- The car is now stepped at exactly 1/60 s whatever the frame rate (`crates/tssm/src/car.rs`,
  in place of `CarUpdate`): the time is added up and the original runs in whole steps. What
  is shown is blended between the last two steps, so the car moves smoothly at any rate.
  Something else moving the car (a reset after a fall) is noticed and taken as it is.
  `TSSM_CAR_FRAME_STEP=1` leaves the game's own stepping, for comparisons.
- Measured headless in `BB01` with accelerate held for 6 s, car position every 0.5 s: with
  the 30 fps cap, at 60, 144 and 240 fps the paths now agree within 0.1 unit. Before, the
  speed after 1 s was 11.1 / 9.2 / 11.7 (cap / 60 / 240), and at 240 it swung between 11.6
  and 17.4 from frame to frame. Frame-to-frame speed at 240 now varies less than before
  (spread 1.8 against 2.8, with 0.8 at 60, partly the headless run's timing).
- `TSSM_TRACE_PLAYER=1` now also logs the car's position and velocity (`[car]` lines).

### Frame Rate and Field of View in the game's settings screen
- The settings screen (Options, Options) has two more rows under SFX Volume, drawn by the
  game's own text and bar widgets (`crates/tssm/src/options.rs`). Down from SFX Volume, or a
  click, gives them the focus; Left and Right (keys, stick or pad, held to repeat) or a click
  or drag on the bars change the value. Each has eleven values, one per bar:
  - Frame Rate: 60, 75, 90, 120, 144, 165, 200, 240, 300, 360, 500. Takes effect at once and
    is saved to `config/fps.txt` (the frame rate can now change while the game runs). A
    `display` setting there shows as the nearest value until changed.
  - Field of View: 60 to 110 in steps of 5, the game's own being 75, in degrees across a 4:3
    picture (wider windows see more at the sides). Saved to `config/fov.txt`, also read
    from `TSSM_FOV`. Only views of 70 degrees or more (play; driving is 80) are changed, by
    the difference from 75; cutscene shots (25 to 45) keep theirs (`aspect.rs`).
- Fixed after the first play: leaving the screen while Frame Rate or Field of View had the
  focus left its title, numbers and bars on screen in the game. The screen slides up off the
  top when it is left, its widgets still listed as on show; what is drawn here now moves
  with SFX Volume, and the focus goes back to the game once the screen has slid away.
- Fixed after the first play: Frame Rate and Field of View bounced along with SFX Volume
  when the cursor was on it (the game bounces the row under the cursor), since they were
  placed from it. They are now placed from the screen's slide: the middle one of the three
  rows' offsets from their places.
- Fixed after the second play: with SFX Volume chosen, Frame Rate and Field of View looked
  darker. They took SFX Volume's heavier shadow (a text widget's shadow has its own opacity,
  at 0x6F, 255 with the focus and 100 without) and its size while bouncing (0x64). They
  now get their own: measured over both rows, the same colours with Music Volume or SFX
  Volume chosen.
- After the third play: the five rows are spread evenly to fit in the panel (tops 172 to
  324, 38 apart). The game's three are drawn and clicked 18 to 22 higher than the game puts
  them (`row_shift`, also applied where menu.rs reads widgets' places). Frame Rate and Field
  of View bounce with the focus as the game's rows do: up 5 and back over 0.3 s of every
  0.5 s, measured from a game row (its timer is at 0x64, which is not a size).
- Not checked: the main menu's settings screen (only the pause menu's was tried), how the
  heads-up display's burgers look at a wide field of view.

### Menus: settings' bars follow the mouse at once
- A click on a volume's bar sets that level at once, and holding the button and moving
  across the bars slides it. Min, Max, Off and On also change at once. Before, the clicks
  were queued as presses of Left or Right, one every few frames. The setting's widget is
  now sent Right or Left directly, as often as needed, which is what the pad does.
- `TSSM_MOUSE` gained `press` and `release` (left button down and up) for drags; `click`
  now also releases.

### Level select: SpongeBob walks to the token under the cursor again
- Moving the choice to the token under the cursor handed the game's focus straight to it,
  which skipped the game's step and the walk sound. Now the focus goes to the token next
  to it and the last step is the game's own Right or Left, so it walks over with the sound.

## 2026-10-09

### The flashing light at the Krusty Krab (looked into; nothing changed in how it is lit)
- Reported: the light on the Krusty Krab in the first level jumps about on the building.
  It is the police car's flashing light by the "do not cross" tape: an orange light that
  rises and falls about once a second and is worked out per vertex, as on the console, so
  on the building's large faces it shows as patches that come and go. Measured at the
  spawn point with the camera still: the ground's average red goes 150 -> 247 -> 150 once a
  second, the same with the old CPU path and the vertex shader, and at 60 and at 240 fps
  (so it follows time, not frames). Not compared with a console or another emulator.
- The vertex shader's position is now marked invariant, so that a surface drawn twice (a
  light or shadow pass over the first) gets exactly the same depth both times. No case of
  the two fighting was seen; it is a precaution.

### No "insufficient free space" notice when all three save slots are used
- With three save files on the card the game opened with "There is insufficient free space
  on the Memory Card in Slot A ... requires 1 file and 9 blocks to save new data". The
  game's own start-up check (`iSGTgtHaveRoomStartup`) answers "no room" whenever all three
  of its save files exist, whatever the card has free. The third of its three looks for a
  save file is now told there is none (`before_room_check` in `card.rs`), so the check goes
  by the card's free space. Reproduced on the desktop with a copy of the phone's three
  saves: the notice came before, and does not now. Not checked: the save and load menus
  with three saves afterwards.

### Stutters: slow frames are logged; compiled pipelines are kept between runs
- A frame that takes more than 2.5 times as long as those before it (and over 12 ms) is
  logged as "[stutter] frame N took ...", with the time asleep, decoding and in the
  renderer, the shaders, pipelines and textures made in it, and the vertices put into the
  display list cache. Level loads and movies show up there too.
- In a session on the desktop (first level, 200 to 360 fps) the stutters felt were GPU
  pipelines being made: one took 60 ms, others 4 to 16 ms. Every pipeline of the new
  vertex shader path was new to the graphics driver.
- The vertex stage is now one shader module shared by all materials instead of a copy in
  each material's module, so the driver sees the same vertex shader every time.
- On Vulkan the driver's compiled pipelines are kept in `cache/wgpu_pipeline_cache_...`
  (wgpu's pipeline cache; written at most every 10 s by a thread of its own, read at the
  start). Measured: the 20 pipelines made as `BB02` loads took 120 ms in a first run and
  14 ms in the next. A pipeline never met before still takes its full time once.
- The display list cache filled up and was emptied every 300 to 3,000 frames, and only
  about 55% of the vertices came from it. The game writes many of its lists, and the arrays
  they index, afresh every frame to a different address, most of them the same as before;
  the cache knew lists by address and took those for new ones. It now knows a list by the
  hash of its bytes and the vertex layout, and its vertices by a hash of exactly the bytes
  it reads of the arrays, wherever they are (`shapes`, `checks`, `places` in `gx.rs`). The
  quick check by blocks is kept for arrays that are where they were. Lists that decode to
  the same vertices and indices share one place, one list can have a place for each set of
  vertices it is drawn with, and the buffers are twice the size (1.2 million vertices).
- Measured headless at 240 fps: `BB02` with a scripted walk, 81% of the vertices from the
  cache, 15% to the vertex shader afresh, 4% on the CPU, nothing put into the cache in
  steady play, 235,000 vertices held after 70 s; `BB01` driven by script, 340,000 held
  after 85 s and never full. Picture against the old path checked again as before (`BB02`
  in play and paused, `BB01` paused, 4:3 and 20:9): same result.
- "[stutter]" lines also say which SDK calls the game was last seen in during the frame.
- Still unexplained: in `BB03`, a frame of 23 to 24 ms about every 2,000 frames, each with
  7,000 to 23,000 vertices going into the display list cache but only about 5 ms of
  decoding. The log line now has the time asleep, to tell waiting from work next time.

### Graphics: the vertex shader transforms and lights; display lists are kept on the GPU
- Profile first (phone, display rate, 144 Hz, before any of this): `BB01` driven by script
  77 fps, a frame 10.8 ms decoding graphics commands (4.7 vertices, 1.9 materials, 1.7 other
  draw setup, 1.8 renderer) and 3.4 ms everything else; `BB02` standing 70 fps, 11.7 ms
  decoding (6.7 vertices) and 2.5 ms everything else. `TSSM_TRACE_PERF=1` now prints that
  split per frame, with the time asleep and in the sound mixer.
- Vertices drawn in perspective go to the GPU as the game sent them (`RawVertex`), and a
  vertex shader (`vs_gx` in `render.rs`) does what `VertexJob::run` did on the CPU: position
  and normal matrix, the two lit color channels with up to eight lights, texture coordinate
  generation, projection. Still on the CPU: flat (2D) drawing, the heads-up display's
  models, and vertices that choose their own matrix (skinned characters, about 2,000 to
  3,500 a frame in `BB02`).
- Display lists are cached (`cached_list` in `gx.rs`): a list that holds only draws is
  decoded once into buffers that stay on the GPU and is drawn from there as one draw for as
  long as its bytes and the parts of the vertex arrays it indexes hash the same (memory is
  hashed in 256-byte blocks, each once per decode). A list is cached the second time it
  comes; one that takes new room three times within 30 frames (an animated model) is
  left out for a while, twice as long each time, and its vertices go to the vertex shader
  afresh. When the buffers are full everything is dropped and built again (see the entry
  above for what filled them at first).
- Per-draw work: everything a draw depends on is worked out once and kept until a BP, CP or
  XF register changes value (`Template`); a draw that follows another from the same state
  only lengthens it. The renderer sets pipeline, bind groups, viewport and scissor only
  when they differ from the draw before.
- The game's glow effect calls RenderWare's `RwGameCubeGetXFBs` every frame, which waits
  for the frame before to be shown by looping. That kept one core fully busy all the time
  (measured: 8 s of processor time in 8 s at 60 fps, 3.6 s after). The wait is now done
  asleep (`before_get_xfbs` in `hle.rs`). The earlier "game code" figures included that
  loop, so the game's own code takes less than they say.
- `TSSM_CPU_VERTICES=1` turns the vertex shader and the cache off (the old path);
  `TSSM_CPU_VERTICES=even` does so on even-numbered frames only, so two screenshots one
  frame apart show the same scene both ways. `TSSM_TRACE_CALLS=1` lists every 5 s the SDK
  calls the game was last seen in.
- Picture, old against new on consecutive frames, at 4:3 and 20:9: the title menus are
  identical pixel for pixel; the pause screens of `BB02` and `BB01` and `BB02` in play
  differ in 9 to 34% of the pixels, by one or two levels of 255 (mean 0.06 to 0.18), and in
  at most 0.03% by more than 16; `BB01` while driving was only compared by eye (the car
  moves between the two frames).
- Desktop, in a window, `BB01` driven by script, dev build: 202 fps (slowest quarter 158)
  against 165 (111) with the old path; decoding 2.5 ms a frame against 3.7.
- Phone (144 Hz, same build, old path by `TSSM_CPU_VERTICES=1` against new, each run
  about two minutes, averages of the two-second samples):
  `BB01` driven by script: 90 fps (slowest quarter 65) -> 143 fps (slowest quarter 139,
  lowest sample 136); decoding 8.9 ms a frame -> 3.6 ms (vertices 4.5 -> 0.55, materials
  1.8 -> 0.5, other setup 0.45 -> 0.15, renderer 1.6 -> 1.0).
  `BB02` standing: 84 fps -> 144 fps; decoding 8.7 ms -> 2.7 ms. (The two `BB02` runs did
  not show the same scene: 71,000 vertices a frame in the old run, 43,000 in the new.)
  Whether the phone's Diablo mode was on is not known; the processor's sensor read 99.6
  degrees after the runs.
- On the phone a pipeline takes 20 to 40 ms to make the first time it is needed: 5 such
  frames in two minutes of `BB02`. They are kept in the pipeline cache for later runs;
  not measured: how long they take from the cache there.

### More than 60 frames a second
- The game shows one frame per retrace and steps by the time that has passed (zGameLoop
  measures it, with only an upper limit of 0.1 s), so the retrace now comes at the display's
  refresh rate instead of 59.94 Hz (`retrace_ticks` in `host.rs`). `config/fps.txt` or
  `TSSM_FPS` sets it: `display` (the default, also on Android), `60` for the console's rate,
  or a number from 20 to 1000. Headless runs keep 60 unless `TSSM_FPS` is set. Movies keep
  the console's rate. Test scripts, `TSSM_WARP` and the "[vi] retrace" log lines still count
  retraces at 60 a second, so the frames per two seconds in those lines give the frame rate.
- With another rate than 60 the desktop window presents with Mailbox (newest frame, no
  queue), as Android already did. Android's performance hint target follows the rate.
- Measured headless in `BB02` with the same scripted walk and jump at 30, 60, 144 and
  240 fps (`TSSM_TRACE_PLAYER=1` logs the player's position every frame): 3 s of walking
  covers 10.10 / 10.08 / 10.04 / 10.06 units, and a walk with a jump ends within 0.1 unit of
  the same place. The difference between 60 and 240 is no larger than between 30 and 60,
  which the console itself ran at.
- Seen running: desktop window on the 360 Hz display at about 100 to 150 fps in `BB02` and
  `BB03` (dev build, with another build running beside it); the phone at 110 to 144 fps in
  the same levels. Not checked: everything else that might count frames instead of time
  (enemies, the driving and sliding levels, bosses, cutscenes, particle effects), the heat
  and battery use on the phone, and whether a frame rate that does not divide the display's
  looks even.

### Android: no more crash when the second level of a session loads
- Going from one level to the next on the phone hung on the loading screen and then
  closed. The performance hint session (`hint.rs`) is reopened when a new thread shows
  frames, and the list still named the first load's loading-screen thread, which had ended:
  the system refuses such a session, and the code then asked it to close a null one, which
  stops the app ("APerformanceHint_closeSession: session is nullptr" was the log's last
  line). Dead threads are now dropped from the list, and a refused session is not closed.
- Checked on the phone with `TSSM_BOOT=BB02` and `TSSM_WARP=BB03@2500`: `BB03` loads and
  runs, with the session reopened for the 4 threads still alive.

### Touch controls go away again for the keyboard and mouse
- A finger that landed on no control brought the touch controls back without counting as
  the touch screen being in use, so the keyboard and mouse never put them away again. Every
  finger now counts.

### Android asks for the display's highest refresh rate; Yes and No on notices clickable
- Android: the window asks the system for the display's highest refresh rate
  (ANativeWindow_setFrameRate with the highest of Display.getSupportedModes), so the pointer
  can move at 144 Hz rather than the 60 the system settles on for a 60 fps game. Logged as
  "[window] asked for N Hz".
- Mouse menus: Yes and No under a notice (the autosave notice after New Game) follow the
  cursor and can be clicked. The notice's box, which also has the focus and lies under
  both, used to count as the item under the cursor; an item inside a box now wins.

### Android: the window is shown by a thread of its own (presenter)
- The game no longer puts its frames in the window itself. It copies each finished frame
  to a texture, and a presenter thread shows the newest one with the touch controls and the
  pointer over it, whenever there is a new frame or the pointer moves (at most every 2 ms,
  Mailbox). The pointer so moves at the display's rate, not the game's 60 a second, and a
  frame is on its way to the screen the moment it is finished.
- On by default on Android only; TSSM_PRESENTER=1 turns it on on the desktop, 0 off.
- The pointer is redrawn: a plain white arrow with an even black edge.

### Android: less delay between input and picture
- The window presented with vsync and a queue of frames (Fifo), so everything shown, the
  pointer included, was a few frames old. On Android it now uses Mailbox (the newest frame
  replaces the one waiting, no tearing) with a frame latency of 1; the game paces itself by
  its retrace timer. The mode chosen is logged ("[render] present mode"). Desktop unchanged.

### Android: raw mouse everywhere and the game's own pointer
- The mouse is now held (pointer capture) whenever the app has the focus, not only during
  play, so Android's pointer with its acceleration is never used or shown. On menus the raw
  movement moves a pointer the game draws itself (an arrow, in the touch overlay): one
  count is one pixel of a 1080-high screen, as Windows at its default speed without
  acceleration. During play the same counts turn the camera, as on the desktop.

### Touch B and X swapped; Android mouse look no longer slow
- The touch controls' B and X buttons have changed places (B is now the outer one).
- Android: a held mouse turned the camera far too slowly. The system delivers several of a
  mouse's reports as one event with the earlier ones as its history, and only the last was
  counted; now all are. The counts themselves are the mouse's raw ones, as on the desktop.
- TSSM_TRACE_INPUT=1 logs once a second how far the mouse moved and in how many reports
  ("[input] mouse moved ..."), on both platforms, to compare them.

### Android: keyboard and mouse as on the desktop
- A mouse on the phone works as in a window: free on menus (the system's pointer; hover,
  click, wheel, as menu.rs does on the desktop), and held during play
  (View.requestPointerCapture, raw movement from Source::MouseRelative) to turn the camera,
  with its buttons as bound in controls.txt.
- The input layer now tells the touch screen from the keyboard and mouse (it only told the
  gamepad from the rest, and a phone never counted as a keyboard). Using the keyboard or
  mouse puts the touch controls away and shows key pictures and the mouse menus; touching
  the screen brings the touch controls back.

### Controllers: the right face button is B
- Xbox B / PlayStation circle is now the GameCube's B (back in menus), and Xbox X / square
  is X. They were the other way round (mapped by position). Desktop and Android.

### Rumble
- PADControlMotor was a no-op; it now drives the rumble motor of the device in use. The game
  only starts the motor with Vibration on in its options. A weak rumble, which the game
  makes by switching the motor on and off, is bridged over gaps under 70 ms.
- Desktop: every gamepad that can rumble does (gilrs force feedback), while a gamepad was
  the last thing used. Nothing rumbles while playing with keyboard and mouse.
- Android: a controller rumbles through its own vibrator (InputDevice.getVibrator), and
  with the touch controls in use the phone's vibrator runs instead. Needs the VIBRATE
  permission (added to the manifest) and the jni crate (Android only).
- TSSM_TRACE_INPUT=1 logs "[input] rumble on/off".

### Any button skips a movie; the window remembers fullscreen
- A movie that a button can skip is now skipped by any gamepad button, any key and any
  mouse button (iFMVPlay's button mask is widened; BinkDoFrame marks a movie as playing).
- The game starts fullscreen the first time. F11 / Alt+Enter write the choice to
  config/window.txt ("fullscreen" or "windowed"), which the next start follows.

### Movies: no strips of the movie before above and below
- Skipping an intro movie that fills the screen left its top and bottom rows on show above
  and below the next one, which is less tall. A movie is drawn straight into the display
  buffer in guest memory, only on its own rows; the game clears the screen between movies
  with a GPU frame, and GPU frames are shown from the GPU and were never written to that
  buffer. The buffer is now made black wherever the GPU copies a frame to it
  (`show_display_buffer` in `host.rs`).
- Reproduced headless by pressing Start during the underwater movie: the rows above and
  below the next movie were that movie's blue before, and are black now. Without a skip
  they were black already.

### Menus: the mouse, Escape, and hints without controller buttons
All of this is while the keyboard and mouse are what was last used. After any gamepad input
the menus are the original game's again, and they always are on Android.
- The mouse works in the menus (`crates/tssm/src/menu.rs`). Moving the cursor onto an item
  gives it the focus, a click chooses it (A) and the wheel presses Up and Down. While a menu
  is on show the mouse's buttons are not what `controls.txt` binds them to: the left one
  clicks and the right one does nothing. A cursor that is not moving changes nothing, so
  the keys work as before.
  - It goes through the game's own event links: the items of a menu are the widgets the
    focus can reach from where it is, and hovering sends the same focus-off and focus-on
    events that Up and Down would.
- Escape goes back a screen (B) in every menu, and from the level select screen (the pause
  screen) and the skills menu back to the game.
- The title screen reads "Press any key to continue" instead of "Press Start", and any key
  or mouse button starts.
- On the skills menu the focus does not follow the cursor (the hover wait of 140 ms is
  gone): a click on a skill selects it, and a click on the selected skill tries to buy it.
  The cursor can so cross the list on its way to "Upgrade description". This applies
  wherever a hint stands for something done to the chosen item.
- Hints along the bottom of a menu are drawn without a button's picture and are clicked:
  "Return", "Options", "Accept changes", "Upgrade description". Those that only say what a
  click does are not drawn: "Choose", "Adjust", "Warp to level". (`render_text`, in place of
  the game's text widget drawing.)
- The settings screen (Options, Options): a setting is chosen by a click, not by the cursor
  passing over it. A volume is set by a click on one of its bars, or on Min or Max; rumble
  by a click on Off or On.
- The level select screen:
  - The arrows above and below the level's name are clicked; the wheel does the same.
  - The challenge under the cursor is lit and chosen at once, and none is lit while the
    cursor is on none. The choice goes straight to it: the focus is handed from the chosen
    challenge's name to the cursor's (`choose_challenge`), as Left and Right hand it to a
    neighbour, so the ones between are not passed. The lighting is drawn here
    (`render_model`, `render_image`), since the game fades its own in.
    The place a challenge is clicked is where it is drawn, which is higher than its widget.
  - A click on a coin (empty or not) asks "Warp to this level?", a second click answers
    yes to that and a third to "Are you sure you're sure?". Moving the cursor off the coin
    while it asks answers no. A locked challenge (question mark) is only chosen.
  - "Options" and "Return to game" sit one each side of the middle.
- Elsewhere (texts during play) a controller button's picture is replaced by the key bound
  to it, drawn as a key cap, or by a mouse with a button lit (`glyph.rs`). The pictures
  wait a moment after the keyboard takes over from a gamepad, so that the frame the change
  falls into does not show a gamepad's hint with a key in it.
- The cursor is hidden whenever a level is running and no menu is on show, also where the
  game holds the camera still (a boss fight); it used to show there.
- `config/hle.txt` gained hooks before `zUIMgrUpdate` and on the UI's text, model and
  image drawing (`Hle::Replace`): regenerate. `TSSM_MOUSE` scripts the mouse, Escape and
  the device in use for unattended runs; `TSSM_TRACE_UI=1` lists the widgets on show.
- Checked headless with the scripted mouse and screenshots at 4:3 (some at 16:9): main
  menu hover and click; Load Game; pause options and its Quit question; settings (rumble,
  both volumes by bar and by Max, Accept changes); Escape from settings back to the game
  through each screen; Escape out of the skills menu; on the level select the arrows, the
  wheel, a jump from the last challenge to the first followed by the three clicks that warp
  (the level loaded again), and the cancel on moving off; the title screen's new text and
  a click starting from it; the skills menu, with a fast pass over the list leaving the
  described skill alone and a rest on a skill choosing it; the switch to a gamepad's hints and back, frame by frame.
  Not checked: a real mouse in a window (the scripted one gives positions on the game's
  640 by 480 screen, so the window-to-screen mapping and the hidden cursor in a boss level
  are untested), a real gamepad, the Extras grid with the mouse (Escape leaves it; its
  icons are not clickable), and key pictures in texts during play.

### Saving: no memory card slot screen, and the autosave notice reworded
- Save Game (pause menu, Options) and Load Game (main menu) no longer ask "Slot A or
  Slot B": they open on the three save slots. There is only ever the one card. Going back
  from the save slots leaves the menu, where it used to return to the slot screen.
  - `pick_slot_a` in `card.rs` stands in for the menu's frame (`zSaveLoad_Tick`) when it
    would be the first frame of the slot screen: once per menu it calls the game's own
    handler for the slot A button, after that it answers "back". With `TSSM_NO_CARD=1`
    the game's own screens are untouched.
  - A hook can now decide per call whether it replaces the original (`Hle::Replace` in
    the runtime). `config/hle.txt` changed (`zSaveLoad_Tick`, `zSaveLoad_SaveLoop`,
    `zSaveLoad_LoadLoop`), so the generated code has to be regenerated.
- The notice New Game shows before offering to create a save file now reads "The game
  uses an autosave feature. While playing, do not power off your device or close the
  game." instead of naming the Memory Card and the Nintendo GameCube. The text is changed
  as `MN/mnus_US.hip` is read (`autosave_notice` in `dvd.rs`); the disc files are not
  modified, so the phone's copy needs no update.
- Checked headless at 4:3 with screenshots, on a copy of the saves: New Game shows the new
  notice; Save Game in `BB01` opens on the three slots, B returns to the options menu,
  and saving into the second slot wrote `GGVE78-SpongeBobMovie02.gci` and returned to the
  level; Load Game opens on the slots and B returns to the main menu. Not checked: the
  Android build (not rebuilt), loading a save through to the level, and the other messages
  that still name the Memory Card (e.g. while saving, or when a save fails).

### State of the game: every level boots; half played
- Played by the user on the desktop and reported working: `DE01`, `DE02`, `TT01`, `TT02`,
  `B101` (boss), `TR01`, `TR02`, the last four with `TSSM_UPGRADES=1`. Two short stutters
  were seen in `TT02`; not found in the log (it samples every two seconds) and not explained.
- The other 23 levels (`TR03`, `JK01`-`02`, `GG02`, `B201`, `BL01`-`04`, `B301`, `PT01`-`03`,
  `AM01`-`04`, `B401`, `B402`, `FB01`-`03`, `SC02`) were each started headless with
  `TSSM_BOOT` and run for 50 s: all loaded and kept presenting frames, with no stop, stall
  or bad jump, and the graphics survey named nothing beyond fog and culling. A screenshot
  of 12 of them was looked at and showed nothing wrong, the boss health bar in `B201` and
  `B301` included. None of these was played; none had a camera tilt above 5 degrees in the
  part reached.

## 2026-10-08

### Camera: the levels' own downward tilt reduced
- Levels have triggers that tilt the camera down while the character is inside them (the
  game's "camera tweaks"), near drops and on steep slides. In the sliding level `TR01`
  several stack up to 45 degrees on top of the usual 15, and the view was seen at 75
  degrees down; with the mouse this fights the player's own aim. Only a share of each
  tilt is now applied: `camera_level_tilt` in `config/controls.txt`, 0.35 by default, for
  the mouse and a gamepad alike (1.0 is the original game, 0 none). The distance change
  that comes with a tweak is kept. `config/hle.txt` changed (`zCameraTweakGlobal_Add` is
  hooked, `before_tweak` in `camera.rs`), so the generated code has to be regenerated.
- `TSSM_TRACE_CAMERA=1` logs the view's angle with the game's camera state whenever it
  turns by 3 degrees, and each level tilt as it is asked for.
- Found from a trace of the user playing `TR01`. Checked headless only at the level's
  start: tilts of 10 and 5 degrees were taken as 3.5 and 1.75 and the camera's angle came
  to 18.5 degrees where it was 25. The user then played `TR01` with it: the 45 degree
  tilts at the large drops were taken as 16, and the user called the result great.
  `TT01` has one such trigger (20 degrees); `TT02` and `B101` have none.

### Testing: all upgrades
- `TSSM_UPGRADES=1` gives both characters every move at its upgraded level and all the
  health slots, so that every part of a level can be reached when starting in it with
  `TSSM_BOOT`. It calls the game's own cheat routines (`AllMovesCB`, `AllUpgradedMovesCB`,
  `MaxHealthSlotsCB`) a second after a level starts running, and again if the levels are
  ever found lower (`upgrades` in `hle.rs`).
- Checked headless in `TT02`: the eight move levels went from 1,0,0,0,1,0,0,0 to 2 each and
  the two health levels from 0 to 3, once, and stayed. Not checked: that each move then
  works when used, and what the upgrade menu shows.
- The game saves these like earned upgrades: playing with the option on and a memory card
  in writes them into the save at the next autosave. The save as it was before is in
  `saves-backup-2026-10-08/`.

### Widescreen: the heads-up display at the screen edges
- On a wide picture the health burgers and the manliness meter sat in the middle 4:3 part.
  They now go to the left and right edges, as far from the edge as on a 4:3 screen.
  - The game draws its display widget by widget (`xhud::render` and the widgets' `render`
    functions). Hooks before those leave a mark at that point of the GPU stream
    (`Gx::mark_hud`), and the game's next call after the display, `zGameModeGet`, marks the
    end. Draws between marks carry their widget's number.
  - When the frame is shown, a widget that lies wholly in the left or right half of the
    4:3 picture is moved to that edge, all of its parts together; one that straddles the
    middle stays there. Its models (the burgers) are also brought back to their 4:3 size.
  - Only what the game draws through its display system is affected: menus, the pause
    screen and text boxes are placed as before.
  - `config/hle.txt` changed, so the generated code has to be regenerated once.
    `TSSM_HUD_CENTRE=1` keeps the display in the middle.
- Checked headless in `BB03` at 4:3, 16:9 and the phone's shape: burgers top left, meter
  top right, same distance from the edges; the pause screen and a text box in `BB02` are
  unchanged. Not seen: other widgets (treasure chests, boss health, the driving levels'
  meters), and how the display sits under the touch controls, which share those corners.
- Known issues: the "HUD not anchored" entry is resolved by this.

### Android: performance hints; the driving level holds 60
- The phone ran the game's threads far below full speed whenever the screen was not being
  touched: the game works for part of a frame and sleeps until the next retrace, which
  looks like a light load. The same view took 8 ms to decode in one stretch and 16 ms in
  another. The game now tells Android how long each frame's work took and that it has to
  fit in 12.5 ms (`crates/tssm/src/hint.rs`, the NDK's performance hint session, for the
  threads that show frames and the vertex helpers; Android 13 and later, looked up at run
  time). Time slept waiting for the retrace is left out of the figure.
- With that, three vertex threads are the default on Android as well.
- Measured on the phone in `BB01` while the user drove for two minutes: 67 of 70
  two-second samples at 60 frames a second, 2 at 55-59, 1 at 26 while loading; the busiest
  frames (9,400 draws, 163,000 vertices) took 9.7 ms to decode, where the same load took
  about 20 ms this morning. All eight cores sat at their top frequency meanwhile, so
  expect more heat and battery use than before; not measured.

### Driving levels at 60 frames a second
- The game holds its driving levels (seen in `BB01`, "Sandwich Driving 101") to 30 frames a
  second by asking RenderWare to show each frame for at least two retraces: they were too
  much for the console. The host now sets that minimum (`_RwDlRetraceMinCount`) back to one
  at every retrace (`uncap_frame_rate` in `host.rs`), so they run at 60 like the rest.
  `TSSM_ORIGINAL_FPS=1` keeps the game's cap.
- Checked headless: `BB01` presents 120 frames every two seconds instead of 60. On the
  phone it holds 60 except in the busiest stretches (around 9,000 draws and 150,000 or
  more vertices a frame), where it runs at 40-46: decoding takes about 18-21 ms there,
  13 ms of it the vertices. Not checked by the developer side: that the game runs at the
  same speed as before (it should, since it steps by elapsed time).

### Performance: a leaner vertex loop and safer sharing between threads
- Aim: hold 60 frames a second in the busiest stretches of `BB01` on the phone, which need
  decoding to come down from about 20 ms a frame to about 12.
- Timing the vertex loop with parts of it switched off (desktop, one thread, start of
  `BB02`) showed where its 70 ns a vertex went: about 20 ns in the colors, 10 ns in texture
  coordinate generation, 3 ns in storing the vertex and none in waiting for memory. So the
  size of the vertex is not the problem, and moving the work to the GPU was not started.
- What varies by draw but not by vertex is now decided before the loop (`VertexJob::run`):
  which texture attributes and coordinates exist, their scales, and for each channel's
  color and alpha where material and ambient come from and which lights count. Colors are
  computed four components at a time instead of by loops over part of a color, and decoded
  by multiplying by a reciprocal instead of dividing (results may differ in the last bit).
  One thread now takes about 54 ns a vertex there instead of 70.
- Threads share the work differently: every thread, the game's included, takes the next
  batch of tasks until none are left, so the game's thread never waits for a helper to
  start and slow helpers can no longer make a frame take longer than one thread would.
  Three threads take about two thirds of the time of one on the desktop.
- Android still defaults to one thread. Whether the new sharing behaves on the phone is
  not measured: the phone was unplugged. Pictures of `BB01`, `BB02` and `BB03` look as
  before on the desktop.

### Performance: vertex work on several cores
- Reported: the sandwich driving level dips on the phone. That level is `BB01` ("Sandwich
  Driving 101"); `BB02`, the first level, is "No Cheese!", and the first measurements were
  taken there by mistake. Measured in `BB02` with `TSSM_TRACE_PERF=1`: 9.5 ms of each 16.7 ms frame went to decoding the game's
  graphics commands on the game's thread, only 1.3 ms of it in the renderer. About half of
  the decoding is the per-vertex work (82,000 vertices in 5,800 draws a frame there).
- A draw's vertices are no longer worked out on the spot. The draw is queued with a copy of
  everything its vertices depend on (`VertexJob` in `gx.rs`), and each time the decoder is
  about to hand control back, or needs the vertices (a framebuffer copy, the end of a
  frame), the queued draws are transformed together on a pool of threads
  (`finish_vertices`; `rayon` is a new dependency): the game's thread and two helpers on
  the desktop. Large draws are split between threads. `TSSM_GX_THREADS` sets the number.
  **On Android the default is one thread**, because the threads made the phone slower:
  with six, every part of the decoding ran several times slower per item and the busiest
  stretches of `BB01` fell to 20 frames a second; with three it swung between 26 and 60 at
  the same load; with one it gives 60 in lighter areas and 40-46 in the busiest. The
  phone's cores were clocked at about 1 GHz of 3.6-4.6 while this happened, which fits its
  governor only speeding up a thread that is kept busy (the cause is an inference; the
  three measurements are not). A sampling profiler could not be run: the phone's kernel
  offers no perf events.
- Normals are no longer fetched and transformed for draws that have lighting switched on
  but no lights in their mask, which is how all of the level geometry is drawn.
- A draw's material (shader, textures, samplers) is kept from the draw before while no BP
  register has changed value and no texture contents may have changed, instead of being
  worked out for every draw.
- `TSSM_TRACE_PERF=1` also says how the decoding time divides between draw materials, the
  rest of a draw's setup and the vertices, with draw and vertex counts.
- Measured on the desktop (dev build, headless, start of `BB02`): decoding went from about
  12.0 ms to 9.1 ms a frame, the vertex part from 6.1 ms to about 3.5 ms. The threads help
  less than their number suggests (three are nearly as good as six), which points at
  memory traffic: every vertex is 112 bytes. Shrinking the vertex and caching draw
  materials are the next steps. Pictures of `BB02`, its pause screen and `BB03` look as
  before. With the material cache on top, decoding there is about 7.7 ms a frame.
- On the phone, `BB01` runs at a steady 30 frames a second (`BB02` runs at 60), and with
  the threads but before the material cache it fell to 22-28 in the busiest stretches,
  where a frame has some 10,000 draws and 200,000 vertices and decoding took over 30 ms.
  In those stretches every part of the decoding ran about twice as slowly per item as a
  few seconds earlier, which suggests the phone's processor was not running at full speed
  (not confirmed).
- To test a level without playing up to it, put `TSSM_BOOT=BB01` in `env.txt` on the phone
  (it is there now, with `TSSM_TRACE_PERF=1`).

### Fixed: models on the pause screen too big on a wide picture
- The token and the row of question marks on the pause screen's level list spilled out of
  their box on the phone (reported from a screenshot at 20:9). The game sizes the models it
  draws over menus by the width of the camera's view, so the widened view made them larger
  in both directions: 1.33 times at 16:9, 1.66 times on the phone. They are now brought
  back to their 4:3 size about their own origin before being moved with the flat drawing
  (`draw` and `end_frame` in `gx.rs`). The earlier fix only moved them.
- `TSSM_DUMP_FRAME` lists the position matrix of every draw in perspective.
- Checked headless on the pause screen of the first level at the phone's shape against
  4:3: token, question marks and box line up the same. A frame of play looks unchanged.
  Not looked at: other models drawn over flat drawing (chests, the health display).

### Android: controllers and hiding the touch controls
- Physical controllers work on Android (USB or Bluetooth). Same layout as on the desktop,
  by position: bottom face button A, left B, right X, top Y, triggers L and R, both
  shoulder buttons and Select Z, Start, d-pad, left stick movement, right stick camera
  (the game's own camera, with the same axis snap). The controller touched last counts.
  - To get at sticks, triggers and the d-pad the Android build no longer uses winit's event
    loop: `crates/tssm/src/android.rs` reads the activity's events itself (window surface,
    fingers, controllers, keyboards). winit passes on a controller's buttons only and
    reports stick movement as a finger in the top left corner of the screen. The desktop
    is unchanged. The renderer now asks for the window's size through a callback instead
    of holding a winit window (`WindowSurface::from_parts` in `render.rs`).
  - A keyboard attached to the phone uses the bindings built into the build
    (`config/controls.txt` as it was when building).
- Using a controller puts the touch controls away completely; touching the screen brings
  them back. (On the desktop too, with a touch screen.)
- A small eye button right of Start hides the touch controls' outlines for playing by
  touch with a clear picture: they keep working where they were, and only the eye stays,
  faintly, to bring them back. Not remembered between runs.
- The phone now has today's fog, culling and loading-screen fixes as well.
- Checked on the phone: boots to the title screen with the new event loop, the eye button
  hides and the outlines are gone in a screenshot, a simulated controller's Start button
  reaches the game and hides the touch controls, and events from a simulated controller
  with sticks arrive. Confirmed by the user with a real controller: it works, and the
  touch controls go away when it is used; the eye button works too. The phone (RedMagic) shows its own "Gravity X" dialog for a few
  seconds when a controller connects; input reaches the game once that is gone.

### Graphics: fog and culling
- Fog (`tev.rs`, parameters in `tev_params` in `gx.rs`). The console derives a pixel's
  distance from its depth value and fades the color toward the fog color past a start
  distance; the generated shaders now do the same, for the linear curve the game uses and
  the four exponential ones. 22 of the 33 levels surveyed turn fog on. It only shows on distant
  scenery, as in the original. `TSSM_NO_FOG=1` draws without it, to compare.
- Face culling. The game asks for one side of every triangle to be dropped; both were drawn
  before. `TSSM_NO_CULL=1` draws both again, to compare.
- The BP mask register (0xFE) is honoured: a masked register write changes only the bits
  the mask allows.
- `TSSM_SURVEY_GX=1` lists, once each, the GPU features a run uses that are not modelled
  (indirect textures, color-channel swaps, lines and points, depth textures, logic
  operations), and the fog and culling settings. Booting every level for 30 s with it
  found only fog and culling in use; effects later in a level may still need the others.
- Checked by comparing screenshots of six levels before and after. They match apart from
  the fog on distant scenery. Confirmed by the user on the phone: fog looks right.

### Fixed: freeze on the loading screen
- Going into the combat arena from the second level (and, less often, any other load) could
  hang on the loading screen for good. RenderWare queues three frames and its submit
  function sleeps when the queue is full, then carries on without checking again. The game
  has two threads that submit: the main one and the one that animates the loading screen.
  On the console the loading thread has always finished its last frame before the main
  thread draws its first, because seconds of level setup lie between; here that setup can
  be over within one retrace, both threads end up asleep on the full queue, both are woken
  for one free slot and both take it. The queue's bookkeeping is then wrong and the display
  waits for a frame the GPU is never let through to. `before_show_raster` in `hle.rs` now
  makes a thread that arrives while another is asleep in the submit function wait until it
  has gone. Whether it happened depended on timing, which is why it showed up after a slow
  moment and not every time.
  - Checked by running the load under full CPU load, three copies at once: 8 of 48 runs
    froze before the change, 0 of 24 after. The warp from the second level into the arena
    (`AM03`) now arrives. Confirmed fixed by the user on the phone.
- `OSIsThreadSuspended` is implemented. With `TSSM_SURVEY` it used to answer "no" every
  time, which let the game suspend the loading thread again and again and never resume it.
- Periodic alarms no longer replay every tick they missed after a stall; like the console's
  own alarm code they go on from the next tick that is still in the future.
- The stall report (printed when no frame appears for 6 s) now includes the last 96
  breakpoint, token and interrupt events of the GPU queue, because this kind of problem
  goes away when every event is printed as it happens.
- `TSSM_WARP=AM03@2000` sends the player through a portal to another level at that retrace,
  for reproducing problems that only happen between levels.
- All 34 levels were booted directly for 40 s each to look for crashes: none stopped with
  an error. That is a boot check only; nothing in them was played.

### Fixed: crash loading the second level
- The game ran out of memory loading "I'm Ready... Depression" (`BB03`) and stopped itself.
  The port gave it the linker's fallback arena, which ends a megabyte short of the top of
  memory; a retail console gives the game everything from just above its stack up to the
  disc's file table at the very top. `OSInit` (`os_init` in `hle.rs`) now does the same.
  The first level happened to fit in the smaller arena.
- `OSReport`, the game's debug console, now goes to the log as `[game] ...`, and `OSPanic`
  stops with the game's own message. That is how the cause showed up ("Out of memory
  crash ... Heap overlaps malloc pool").
- Every level load is logged as `[scene] loading XXXX`.
- `TSSM_BOOT=BB03` starts directly in a level, skipping the menus (level names are the HIP
  file names under `disc/files`). It hooks `zSceneInit`, so `config/hle.txt` changed and
  the generated code has to be regenerated once.

### Saving
- Memory card (`crates/tssm/src/card.rs`). Slot A now holds a 251-block card; slot B is
  empty. The game's `CARD` calls are answered natively and each file on the card is one
  `.gci` file in the `saves` folder (the format Dolphin imports and exports), so saves can be
  copied, backed up or swapped with an emulator. On Android the folder is `files/saves` in
  the app's private storage, which uninstalling the app deletes.
  - With a card present the game no longer shows the "no memory card" prompt at boot; New
    Game asks whether to create a save file, and Load Game lists it.
  - `TSSM_NO_CARD=1` gives the old behaviour (both slots empty). The unattended input
    script in the notes was timed for that, so it needs this variable now.
  - `TSSM_TRACE_CARD=1` logs every card call and its result.
- The console clock is real: `OSGetTime` counts from the start of 2000 in local time instead
  of from the start of the game, and `OSTicksToCalendarTime` is implemented, so saves carry
  the right date. `OSCheckActiveThreads` is answered too; nothing is left that needs
  `TSSM_SURVEY` on the way into the first level.

### Android
- First Android build: installed and run on a phone over USB (Android 16, arm64, Vulkan).
  Boots through the menus into the first level at the phone's full 20:9 width, using the
  widescreen code as it is.
  - `toolsndroid.ps1` builds the library, wraps it in an APK, signs it with the debug key,
    installs it and starts it. `-PushDisc` also copies the extracted disc (needed once);
    `-Release` builds optimised. No Gradle or Java code: the system's NativeActivity loads
    `libtssm_android.so` (`crates/android`, with its `AndroidManifest.xml`).
  - The host is now a library (`crates/tssm/src/lib.rs`) with `main.rs` a thin wrapper, so
    the same code is the desktop executable and the Android library. The debug variables
    are still documented at the top of `main.rs`.
  - Game data lives in the app's private folder (`files/disc`), not on shared storage: the
    app is refused files that were copied to its shared folder over USB. `log.txt` there
    holds what the desktop prints; `env.txt` takes debug variables, one `NAME=value` per
    line. Reach them with `adb shell run-as com.campi.tssm cat files/log.txt`.
  - `TSSM_SURVEY` is on by default on Android, because the game otherwise stops at
    `OSTicksToCalendarTime` about 20 s after boot.
  - Leaving the app (home button, another app) pauses the game at its next frame and
    coming back resumes it: Android destroys the window's surface in between, and the
    renderer now takes a new one (`surface_lost` / `surface_replaced` in `render.rs`).
- Touch controls (`crates/tssm/src/touch.rs`), drawn over the picture: a control stick that
  centres wherever the left thumb lands, A/B/X/Y in the gamepad arrangement, R and Z above
  them, L top left, Start (pause bars) top centre. Dragging anywhere else turns the camera
  like the mouse does. They also appear on the desktop if a touch screen is used.

### Window
- F11 or Alt+Enter switches between the window and borderless full screen. Neither key
  reaches the game (Enter alone is still Start).
- Wide and custom aspect ratios (`crates/tssm/src/aspect.rs`). The picture follows the
  window's shape from 4:3 up to 32:9; narrower or wider windows get bars. Three parts:
  - a hook before `RwCameraSetViewWindow` widens the horizontal field of view of the
    cameras that fill the screen, so the game itself draws and culls a wider view (true
    widescreen, not a stretch or a crop);
  - the framebuffer gets proportionally more columns and is shown across the wide picture;
  - flat 2D drawing (menus, HUD, text boxes) is squeezed toward the centre so it keeps its
    4:3 shape. Draws spanning the full width (backdrops, fades) stay full width, and 2D
    drawing that the game captures into a texture is left alone.
  - models the game draws on top of the flat drawing (the token on the pause screen, chests,
    the health burgers) go through the same camera as the world, but the game sizes and
    places them to cover a fixed part of the screen. Anything drawn in perspective after
    squeezed flat drawing has begun in a frame is moved with it, as a whole and keeping
    its shape (squeezing it too made the token narrow). Before this the pause
    screen's token sat away from its glow, stretched. (A first attempt keyed on a 4:3
    projection matched nothing and fixed nothing.)
- Fixed: changing the window's shape in mid-session left the 3D view at the old field of
  view (squeezed or stretched) because the game only sets a camera's view window when its
  own field of view changes. A hook before `RwCameraBeginUpdate` now sets it again
  whenever the picture's shape has changed. `TSSM_ASPECT=16:9@7500` switches shape at that
  retrace, for testing this headless.
- Internal resolution now follows the window height (2x up to 6x of 480 lines) instead of
  being fixed at 2x, so full screen is sharp.
- `TSSM_ASPECT=16:9` fixes the picture shape regardless of the window (and is how headless
  runs test it). Movies stay 4:3.

### Graphics
- Lighting. The GX color channels are now evaluated per vertex on the host, as the console
  does it (`Light`, `Channel` and the channel loop in `crates/tssm/src/gx.rs`): material and
  ambient colors from register or vertex color, up to eight lights per channel, diffuse
  none/signed/clamped, specular and spotlight/distance attenuation, color and alpha
  controlled separately. Normals are decoded and transformed by the normal matrices.
- This also fixes the washed-out terrain: the level geometry is prelit and passes its
  vertex colors in as the *ambient* color with no lights, which was ignored before (sand and
  rock came out near white). Characters and props are lit by four directional lights.
- `TSSM_DUMP_FRAME` now also lists each draw's channel count, ambient colors and the
  lights it uses.

- Fixed the square outline in the top-left corner. For the character shadow the game
  borrows the top-left 128x128 pixels of the framebuffer: it saves them to a texture, draws
  the silhouette there, captures it, then draws the saved pixels back unfiltered with
  coordinates half a texel late. At 2x internal resolution that half texel was a one-pixel
  shift with the far edges wrapping round. Unfiltered sampling of a framebuffer copy now has
  the half texel removed (`map_texture` in `gx.rs`).

### Performance
- The view towards the middle of the first level dropped to 30 fps: about 5,700 small draws
  and 78,000 vertices per frame, which the host expanded to some 22 MB of triangle-list
  vertices and as many GPU draw calls every frame. Now steady at 60 fps there (headless
  measurement; graphics decoding went from about 29 ms to 11 ms per frame):
  - indexed drawing, so each vertex is stored once;
  - consecutive draws with the same material and state are merged into one draw call, and
    equal shader parameter blocks are shared (`Frame::push_draw`, `Frame::push_params`);
  - frame buffers are reused instead of regrown every frame;
  - matrices are read once per draw instead of once per vertex;
  - the host crate (`tssm`) is built at `opt-level = 3` even in dev builds (`Cargo.toml`).
- `TSSM_TRACE_PERF=1` logs every 2 s how long graphics decoding and the renderer took.

### Controls
- Gamepad support (`gilrs`). Pads can be plugged in at any time. Fixed layout: bottom face
  button A, left B, right X, top Y, triggers L/R, shoulder buttons and back/select Z, left
  stick movement, right stick camera.
- The device touched last decides the camera: a gamepad gives the original game's camera
  unchanged; keyboard or mouse gives the mouse camera.
- Gamepad camera stick snaps to one axis when pushed mostly along it (`CAMERA_AXIS_SNAP`
  in `crates/tssm/src/input.rs`), so turning no longer also zooms.
- Mouse camera rewritten as a rigid orbit: fixed distance from the character, turned only by
  the mouse, no easing and no automatic swing behind the player. Vertical movement orbits at
  constant radius instead of zooming. The game's collision still pulls the camera in when
  something is in the way. Near the ground the camera stops and only the view tilts further.
- Arrow keys now turn the mouse camera (`look_*` actions) instead of driving the C-stick.
- `TSSM_ORIGINAL_CAMERA=1` forces the original camera without a gamepad.

### How the mouse camera works
- Hooks (`Hle::Before`) on `zCameraUpdate`, `xCameraMove` and `xCameraLookYPR`, listed in
  `config/hle.txt`, implemented in `crates/tssm/src/camera.rs`. The move and look hooks set
  the easing time to zero and replace the orbit angle, distance, height and tilt.

## Earlier work (undated)

### Controls
- Keyboard and mouse bindings in `config/controls.txt` (WASD, Space jump, left click spin,
  right click bash, F Sonic Wave, Tab upgrade menu, Esc pause). Mouse is captured only while
  a level is running, not in menus or on the pause screen.

### Graphics
- GX FIFO decoded on the host and drawn with wgpu (Vulkan), 2x internal resolution.
- TEV stages translated to generated WGSL per material.
- EFB copies to the display buffer and to textures; write-mask-aware clears.
- 2D vertex snapping to fix stale one-pixel borders at 2x.
- Movies (Bink, recompiled) shown from direct XFB writes.

### Audio
- AX library runs recompiled; host emulates DSP/AI/ARAM and mixes the 64 voices
  (ADPCM, PCM16, PCM8) to cpal.

### System
- Recompiler (`crates/recomp`): PowerPC including paired singles to Rust, one function per
  guest function, from the symbol-bearing ELF on the disc.
- Runtime: 4 GB guest address space, cooperative guest threads, interrupt-style event pump
  (retrace, DVD, alarms, audio frames, GX events), back-edge polling.
- HLE policy by name prefix (`config/hle.txt`); the rest of the SDK runs recompiled.
- Windowed and headless modes, stall watchdog, scripted input, screenshots, draw dumps.

### State of the game
- Boots through legal screen, memory-card prompt, logo movies, title, main menu, New Game,
  intro movie and cutscene into free play. First level reported fully playable.

## Known issues
- No indirect textures or swap tables; lines and points are not drawn; no fog range
  adjustment. None of these was seen in use in the first 30 s of any level.
- Lighting is checked only in the first level, which uses directional lights; the specular
  and spotlight paths have not been exercised by anything seen so far.
- Faint green lines on sand; dark pentagon tiles on the path not yet explained.
- The user saw the top-left shadow square turn completely black at times. Not reproduced in
  headless runs and not explained; the outline fix may or may not cover it.
- Vertex transform and lighting run on the CPU, about 7 ms per frame in the heaviest view of
  the first level; busier levels may need it moved to the GPU.
- Saving: checked headless only as far as creating a save from New Game and loading it
  back from Load Game. An autosave during play, deleting a save and formatting use the same
  code but have not been watched happening. Not yet run on the phone.
- Gamepad support and the axis snap are untested by the developer side (no hardware);
  confirmed working by the user.
- Button guesses not confirmed: R = Sonic Wave, Z = upgrade menu.
- Widescreen: checked at 16:9 in the title menus and the first level, headless only. Any 2D element
  the game positions over a 3D object would be drawn off target (none seen so far).  3D world
  geometry drawn after the HUD has started would be squeezed by mistake (none seen). The
  fullscreen toggle and live window resizing are untested by the developer side.
- Android: tested on one phone only, by simulated taps and swipes; sound was not listened
  to. Controllers: only simulated ones were tried, and unplugging one is not noticed
  (touch the screen to get the touch controls back). The touch layout cannot be
  rearranged. Only the dev build was measured (60 fps standing in
  the first level, on a Snapdragon 8 Elite Gen 5; slower phones are unknown).

## Planned, not started
- More than 60 fps: done on 2026-10-09 (see above). Still to do: play the levels at a high rate and look for anything that runs by frames instead of by time.
