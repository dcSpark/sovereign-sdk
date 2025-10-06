cmake_minimum_required(VERSION 3.10)
project(ligero_value_validator CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

# Path to Ligero SDK (adjust if needed)
# Default assumes ligero-vm is a sibling directory to sovereign-sdk
set(LIGERO_SDK_PATH "${CMAKE_SOURCE_DIR}/../../../../../ligero-vm/ligero-prover/sdk" CACHE PATH "Path to Ligero SDK")

# Include Ligero SDK headers
include_directories(${LIGERO_SDK_PATH}/include)

# Link directories for Ligetron library
link_directories(${LIGERO_SDK_PATH}/build)

# Add executable
add_executable(value_validator value_validator.cpp)

# Link against Ligetron library
target_link_libraries(value_validator ligetron)

# Set output as .wasm
set_target_properties(value_validator PROPERTIES SUFFIX ".wasm")

# Install target (optional - copies to bins/programs)
install(TARGETS value_validator DESTINATION ${CMAKE_SOURCE_DIR}/../bins/programs)
