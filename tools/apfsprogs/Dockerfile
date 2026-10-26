# External APFS reference tools, used ONLY to create test images and to
# validate images our code has written. apfsprogs is GPL-2.0; it is never
# linked into or copied into winapfs, it runs as a separate checker.
FROM ubuntu:24.04
RUN apt-get update \
 && apt-get install -y --no-install-recommends build-essential git ca-certificates \
 && rm -rf /var/lib/apt/lists/*
RUN git clone --depth 1 https://github.com/linux-apfs/apfsprogs /src/apfsprogs \
 && make -C /src/apfsprogs \
 && git -C /src/apfsprogs log -1 --format='apfsprogs %H %cs' > /apfsprogs.version
ENV PATH="/src/apfsprogs/apfsck:/src/apfsprogs/mkapfs:/src/apfsprogs/apfs-snap:/src/apfsprogs/apfs-label:${PATH}"
WORKDIR /work
