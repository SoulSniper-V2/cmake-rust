cmake_minimum_required(VERSION 4.1)
find_package(Acme CONFIG NAMES Upstream CONFIGS Package.cmake PATHS data/prefix NO_DEFAULT_PATH)
message("found=${Acme_FOUND}|dir=${Acme_DIR}|config=${Acme_CONFIG}|considered=${Acme_CONSIDERED_CONFIGS}|versions=${Acme_CONSIDERED_VERSIONS}|origin=${origin}")
