cmake_minimum_required(VERSION 3.10)
project(ligero_value_validator CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

# Path to Ligero SDK (adjust if needed)
# Navigate from guest/ to parent directory: guest -> ligero -> adapters -> crates -> sovereign-ligero -> parent
get_filename_component(PARENT_DIR "${CMAKE_SOURCE_DIR}/../../../../.." ABSOLUTE)
set(LIGERO_SDK_PATH "${PARENT_DIR}/ligero-vm/ligero-prover/sdk" CACHE PATH "Path to Ligero SDK")

# Determine include directory
if(EXISTS "${LIGERO_SDK_PATH}/include/ligetron/api.h")
    set(LIGERO_INCLUDE_DIR "${LIGERO_SDK_PATH}/include")
elseif(EXISTS "${LIGERO_SDK_PATH}/cpp/include/ligetron/api.h")
    set(LIGERO_INCLUDE_DIR "${LIGERO_SDK_PATH}/cpp/include")
elseif(EXISTS "${LIGERO_SDK_PATH}/../cpp/include/ligetron/api.h")
    set(LIGERO_INCLUDE_DIR "${LIGERO_SDK_PATH}/../cpp/include")
else()
    message(FATAL_ERROR "Ligero SDK headers not found in ${LIGERO_SDK_PATH}")
endif()

# Determine library directory
if(EXISTS "${LIGERO_SDK_PATH}/build/libligetron.a")
    set(LIGERO_LIBRARY_DIR "${LIGERO_SDK_PATH}/build")
elseif(EXISTS "${LIGERO_SDK_PATH}/../build/libligetron.a")
    set(LIGERO_LIBRARY_DIR "${LIGERO_SDK_PATH}/../build")
else()
    message(FATAL_ERROR "Ligero SDK library not found near ${LIGERO_SDK_PATH}")
endif()

include_directories(${LIGERO_INCLUDE_DIR})
link_directories(${LIGERO_LIBRARY_DIR})

# Add executable
add_executable(value_validator value_validator.cpp)

# Link against Ligetron library
target_link_libraries(value_validator ligetron)

# Set output as .wasm
set_target_properties(value_validator PROPERTIES SUFFIX ".wasm")

# Install target (optional - copies to bins/programs)
install(TARGETS value_validator DESTINATION ${CMAKE_SOURCE_DIR}/../bins/programs)
