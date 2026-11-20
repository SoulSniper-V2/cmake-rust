cmake_minimum_required(VERSION 4.1)
set(PACKAGE_VERSION "outer")
        set(PACKAGE_PREFIX_DIR "outer-prefix")
        find_package(Acme 1.3.4 EXACT CONFIG PATHS data/prefix NO_DEFAULT_PATH)
        message("version=${Acme_VERSION}|${Acme_VERSION_MAJOR}|${Acme_VERSION_MINOR}|${Acme_VERSION_PATCH}|${Acme_VERSION_TWEAK}|${Acme_VERSION_COUNT}")
        message("caller=${PACKAGE_VERSION}|${PACKAGE_PREFIX_DIR}")

message("found=${Acme_FOUND}|dir=${Acme_DIR}|config=${Acme_CONFIG}|considered=${Acme_CONSIDERED_CONFIGS}|versions=${Acme_CONSIDERED_VERSIONS}|origin=${origin}")
