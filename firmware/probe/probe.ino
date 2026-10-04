#include <Wire.h>
#include <Adafruit_INA219.h>
#include <UIPEthernet.h>
#include "config.h"

// Initialize INA219 sensor instance
Adafruit_INA219 ina219;

// Network Configuration
byte mac[] = PROBE_MAC;
IPAddress ip(PROBE_IP);
IPAddress dnsServer(PROBE_DNS);
IPAddress gateway(PROBE_GATEWAY);
IPAddress subnet(PROBE_SUBNET);

// Target Python Listener IP on Host/Server
IPAddress targetIP(COLLECTOR_IP);
unsigned int targetPort = COLLECTOR_PORT;

EthernetUDP Udp;

void setup() {
  Serial.begin(9600);
  while (!Serial);
  delay(1000);

  Serial.println("Initializing INA219 Sensor...");
  if (!ina219.begin()) {
    Serial.println("Failed to find INA219 chip! Check SDA/SCL wiring.");
    while (1) { delay(10); }
  }

  Serial.println("Initializing Ethernet Hardware...");
  Ethernet.begin(mac, ip, dnsServer, gateway, subnet);
  delay(2000);

  Serial.print("Nano Static IP Address: ");
  Serial.println(Ethernet.localIP());

  Udp.begin(8888);
}

void loop() {
  // Read physical telemetry values from INA sensor
  float busVoltage_V = ina219.getBusVoltage_V();
  float current_mA = ina219.getCurrent_mA();
  float power_mW = ina219.getPower_mW();

  // Format telemetry reading as JSON payload
  String payload = "{\"v\":" + String(busVoltage_V, 2) + 
                   ",\"i\":" + String(current_mA / 1000.0, 3) + 
                   ",\"p\":" + String(power_mW / 1000.0, 3) + "}";

  // Transmit UDP packet
  Udp.beginPacket(targetIP, targetPort);
  Udp.print(payload);
  Udp.endPacket();

  Serial.print("Telemetry Transmitted: ");
  Serial.println(payload);

  delay(2000);
}
