import os
import socket

UDP_IP = "0.0.0.0"
UDP_PORT = int(os.environ["TELEMETRY_UDP_PORT"])

sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
sock.bind((UDP_IP, UDP_PORT))

print(f"UDP Telemetry Server listening on {UDP_IP}:{UDP_PORT}...")

while True:
    data, addr = sock.recvfrom(1024)
    print(f"Received payload from {addr[0]}: {data.decode('utf-8')}")
