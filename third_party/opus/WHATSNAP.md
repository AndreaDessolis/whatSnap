libopus 1.6.1, from https://downloads.xiph.org/releases/opus/opus-1.6.1.tar.gz
(SHA-256 6ffcb593207be92584df15b32466ed64bbec99109f007c82205f0194572411a1,
matching Xiph's SHA256SUMS.txt). Built by whatSnap's build.rs with Deep PLC and
OSCE (LACE/NoLACE speech enhancement and the blind bandwidth extension,
ENABLE_OSCE_BWE, which only the autotools build defines; build.rs adds it).

Removed from the release to keep the repository small, none of it built here:
doc/, dnn/torch/ (training scripts), and the model data of the features left
off: dnn/dred_rdovae_*_data.c (DRED) and dnn/lossgen_data.c. Turning one of
those on means restoring its files from the tarball.
