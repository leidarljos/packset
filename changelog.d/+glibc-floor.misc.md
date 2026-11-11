Linux release archives are built on Ubuntu 22.04, so packset, packsetd, packset-mcp and packset-embed need only glibc 2.35.
The Open Neural Network Exchange (ONNX) Runtime is compiled from source on that release, which keeps the encoder on the same floor.
Ubuntu 22.04's CMake package is 3.22 and the runtime requires 3.28, so the job installs the CMake 3.28.6 binary first.
That binary is a build tool and is not linked into the archive.
The release job checks every Linux binary and stops when one needs a glibc symbol newer than 2.35.
