#!/bin/bash
set -e

# Squiddish Docker Multi-Architecture Build Script
# Builds and pushes to GitHub Container Registry

# Color output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

echo -e "${GREEN}Squiddish Multi-Architecture Docker Build${NC}"
echo "=========================================="

# Check for required tools
if ! command -v docker &> /dev/null; then
    echo -e "${RED}Error: docker is not installed${NC}"
    exit 1
fi

# Get version from Cargo.toml
VERSION=$(grep '^version = ' Cargo.toml | head -1 | cut -d'"' -f2)
echo -e "${GREEN}Building version: ${VERSION}${NC}"

# GitHub Container Registry details
REGISTRY="ghcr.io"
IMAGE_NAME="phrontizo/squiddish"
FULL_IMAGE="${REGISTRY}/${IMAGE_NAME}"

# Check if logged in to GitHub Container Registry
echo -e "${YELLOW}Checking GitHub Container Registry authentication...${NC}"
if ! docker info 2>/dev/null | grep -q "Username"; then
    echo -e "${YELLOW}Please log in to GitHub Container Registry:${NC}"
    echo "docker login ghcr.io -u phrontizo"
    exit 1
fi

# Create buildx builder if it doesn't exist
BUILDER_NAME="squiddish-builder"
if ! docker buildx inspect ${BUILDER_NAME} &> /dev/null; then
    echo -e "${YELLOW}Creating buildx builder: ${BUILDER_NAME}${NC}"
    docker buildx create --name ${BUILDER_NAME} --use
else
    echo -e "${GREEN}Using existing builder: ${BUILDER_NAME}${NC}"
    docker buildx use ${BUILDER_NAME}
fi

# Bootstrap builder
echo -e "${YELLOW}Bootstrapping builder...${NC}"
docker buildx inspect --bootstrap

# Build and push multi-architecture image
echo -e "${GREEN}Building and pushing multi-architecture image...${NC}"
echo "Platforms: linux/amd64, linux/arm64"
echo "Tags: ${VERSION}, latest"

docker buildx build \
    --platform linux/amd64,linux/arm64 \
    --tag "${FULL_IMAGE}:${VERSION}" \
    --tag "${FULL_IMAGE}:latest" \
    --push \
    .

echo -e "${GREEN}✓ Build complete!${NC}"
echo ""
echo "Images pushed to:"
echo "  - ${FULL_IMAGE}:${VERSION}"
echo "  - ${FULL_IMAGE}:latest"
echo ""
echo "To run:"
echo "  docker run -p 3128:3128 -v \$(pwd)/cache:/cache ${FULL_IMAGE}:latest"
echo ""
echo "To pull:"
echo "  docker pull ${FULL_IMAGE}:latest"
