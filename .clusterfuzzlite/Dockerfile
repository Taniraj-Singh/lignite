# ClusterFuzzLite build image for lignite (Rust / cargo-fuzz).
FROM gcr.io/oss-fuzz-base/base-builder-rust

# Bring the whole repository into the image.
COPY . $SRC/lignite
WORKDIR $SRC/lignite

# The build script that compiles the fuzz targets into $OUT.
COPY .clusterfuzzlite/build.sh $SRC/
