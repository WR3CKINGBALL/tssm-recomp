The SpongeBob SquarePants Movie - Windows port builder
=======================================================

This package does not contain the game. It turns your own copy of the GameCube disc into a
Windows game: it reads the game's code from your disc image, translates it, and builds
TSSM.exe on your computer.

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
  5. Start dist\TSSM.exe.

Your saves go in dist\saves. Settings are in dist\config, and the frame rate and field of
view can also be set in the game (Options, Options).

Controls: see dist\config\controls.txt. A gamepad works too.
