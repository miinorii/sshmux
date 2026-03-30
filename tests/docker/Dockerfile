FROM alpine:3.21

RUN apk add --no-cache openssh-server openssh-client openssh-sftp-server bash coreutils
RUN ssh-keygen -A 
RUN adduser -D -s /bin/bash testuser && echo "testuser:testpass" | chpasswd
RUN mkdir -p /home/testuser/.ssh /home/testuser/documents /home/testuser/photos && \
    echo "hello world" > /home/testuser/documents/readme.txt && \
    echo "second file" > /home/testuser/documents/notes.txt && \
    dd if=/dev/urandom of=/home/testuser/photos/sample.bin bs=1024 count=4 2>/dev/null && \
    chown -R testuser:testuser /home/testuser && \
    chmod 700 /home/testuser/.ssh

# Install the test public key for key-based auth
COPY test_key.pub /home/testuser/.ssh/authorized_keys
RUN chown testuser:testuser /home/testuser/.ssh/authorized_keys && \
    chmod 600 /home/testuser/.ssh/authorized_keys

# Allow password auth + pubkey auth, disable strict modes for test convenience
RUN sed -i 's/#PasswordAuthentication yes/PasswordAuthentication yes/' /etc/ssh/sshd_config && \
    sed -i 's/#PubkeyAuthentication yes/PubkeyAuthentication yes/' /etc/ssh/sshd_config && \
    sed -i 's/#PermitRootLogin.*/PermitRootLogin no/' /etc/ssh/sshd_config && \
    echo "Subsystem sftp /usr/lib/ssh/sftp-server" >> /etc/ssh/sshd_config

EXPOSE 22

CMD ["/usr/sbin/sshd", "-D", "-e"]
