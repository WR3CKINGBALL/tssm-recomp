The SpongeBob SquarePants Movie - Windows port builder
=======================================================

This package does not contain the game. It turns your own copy of the GameCube disc into a
Windows game: it reads the game's code from your disc image, translates it, and builds
"The SpongeBob Movie Game.exe" on your computer.

You need
  - A disc image (.iso or .gcm) of The SpongeBob SquarePants Movie for GameCube, USA
    (GGVE78, revision 1), made from a disc you own. Other versions are refused.
  - Windows 10 or 11, 64-bit, an internet connection for the first build, and about 10 GB
    of free space for the build tools.

How to build
  1. Unzip this whole package into a folder (keep the folders together).
  2. Double-click build.bat and choose your disc image.
  3. If Rust or the Visual Studio C++ build tools are missing, it asks whether to download
     and install them (answer Y). Windows asks for permission for the build tools.
  4. Wait. The first build takes a while (the tools, then the game: about 10 to 30
     minutes depending on the computer). Later builds are much quicker.
  5. It offers the Xbox version's textures, a separate download from
     https://gamebanana.com/mods/648590 - answer Y, download the pack, and choose its zip.
  6. Start "The SpongeBob Movie Game.exe", which appears next to build.bat.

Your saves go in the saves folder and settings in the config folder. The frame rate and
field of view can also be set in the game (Options, Options).

Texture packs made for Dolphin work too: put the zip next to the game, and the game asks
whether to use it when it starts (or put the zip, or its PNGs, in the textures folder).

Controls: see config\controls.txt. A gamepad works too.
