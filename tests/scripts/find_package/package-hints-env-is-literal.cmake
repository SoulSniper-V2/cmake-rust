cmake_minimum_required(VERSION 4.1)
set(ENV{PKG_PATHS} "data/hint")
find_package(Acme CONFIG HINTS ENV PKG_PATHS NO_DEFAULT_PATH)
message("found=${Acme_FOUND}|dir=${Acme_DIR}|config=${Acme_CONFIG}|considered=${Acme_CONSIDERED_CONFIGS}|versions=${Acme_CONSIDERED_VERSIONS}|origin=${origin}")
