# Test SSH server for integration tests.
#
# Usage:
#   docker build -t warp-sshd docker/sshd
#   docker run -d -p 2222:22 --name warp-sshd warp-sshd
#
# Credentials: testuser / testpassword

FROM ubuntu:22.04

RUN apt-get update && apt-get install -y --no-install-recommends \
        openssh-server \
        sudo \
    && rm -rf /var/lib/apt/lists/*

# Create test user
RUN useradd -m -s /bin/bash testuser \
    && echo 'testuser:testpassword' | chpasswd \
    && echo 'testuser ALL=(ALL) NOPASSWD:ALL' >> /etc/sudoers

# Configure SSH
RUN mkdir /run/sshd \
    && sed -i 's/#PasswordAuthentication yes/PasswordAuthentication yes/' /etc/ssh/sshd_config \
    && sed -i 's/#PermitRootLogin prohibit-password/PermitRootLogin no/' /etc/ssh/sshd_config

EXPOSE 22

CMD ["/usr/sbin/sshd", "-D"]
