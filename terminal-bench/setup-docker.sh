#!/usr/bin/env bash
# Terminal Bench Docker + Harbor setup script
# Requires: sudo access (will prompt for password)
# Run: bash terminal-bench/setup-docker.sh
set -euo pipefail

echo "=== Terminal Bench Infrastructure Setup ==="

# 1. Install Docker
echo ""
echo "--- Step 1: Install Docker ---"
if command -v docker &>/dev/null && docker info &>/dev/null 2>&1; then
    echo "Docker already installed and running."
    docker --version
else
    echo "Installing Docker..."
    sudo apt-get update
    sudo apt-get install -y docker.io docker-compose-v2

    # Add user to docker group
    if ! groups "$USER" | grep -q '\bdocker\b'; then
        echo "Adding $USER to docker group..."
        sudo usermod -aG docker "$USER"
        echo "NOTE: You may need to log out and back in for group membership to take effect."
        echo "      Or run: newgrp docker"
    fi

    # Start Docker if not running
    sudo systemctl enable docker
    sudo systemctl start docker

    echo "Docker installed."
    docker --version
fi

# 2. Verify Docker
echo ""
echo "--- Step 2: Verify Docker ---"
if docker run --rm hello-world 2>/dev/null | grep -q "Hello from Docker"; then
    echo "✓ docker run hello-world: PASS"
else
    # Try with newgrp if permission denied
    echo "Trying with newgrp docker..."
    sg docker -c "docker run --rm hello-world" 2>/dev/null | grep -q "Hello from Docker" && \
        echo "✓ docker run hello-world: PASS (via sg docker)" || \
        echo "✗ docker run hello-world: FAIL (try logging out and back in)"
fi

# 3. Verify Harbor
echo ""
echo "--- Step 3: Verify Harbor ---"
if python3 -c 'import harbor; print("Harbor version:", harbor.__version__)' 2>/dev/null; then
    echo "✓ Harbor import: PASS"
else
    echo "Harbor not found. Installing..."
    pip3 install --user --break-system-packages harbor
    python3 -c 'import harbor; print("Harbor version:", harbor.__version__)'
    echo "✓ Harbor installed"
fi

# 4. Pre-pull TB images (avoids Docker Hub rate limiting during runs)
echo ""
echo "--- Step 4: Pre-pull Terminal Bench images ---"
echo "Docker Hub rate-limits anonymous pulls (~100/6h). A full TB run"
echo "(89 tasks x 3 trials) will exceed this. Pre-pulling caches images locally."
echo ""
echo "To pre-pull (run after 'harbor download terminal-bench@2.0'):"
echo "  bash terminal-bench/pre-pull-images.sh"
echo ""
echo "To check which images are cached:"
echo "  bash terminal-bench/pre-pull-images.sh --check"

# 5. Summary
echo ""
echo "=== Setup Complete ==="
echo "Docker: $(docker --version 2>/dev/null || echo 'NOT AVAILABLE')"
echo "Harbor: $(python3 -c 'import harbor; print(harbor.__version__)' 2>/dev/null || echo 'NOT AVAILABLE')"
echo ""
echo "Run verification:"
echo "  docker info && python3 -c 'import harbor'"
echo ""
echo "If docker group isn't active in your session, use:"
echo "  sg docker -c 'docker info' && python3 -c 'import harbor'"
echo ""
echo "IMPORTANT: Pre-pull Docker images before full runs:"
echo "  bash terminal-bench/pre-pull-images.sh"
