terraform {
  required_providers {
    hcloud = {
      source  = "hetznercloud/hcloud"
      version = "~> 1.53"
    }
  }
}

provider "hcloud" {
  token = var.hcloud_token
}

resource "hcloud_firewall" "bench" {
  name = "pibake-bench"

  rule {
    direction  = "in"
    protocol   = "tcp"
    port       = "22"
    source_ips = var.ssh_source_ips
  }

  labels = local.labels
}

resource "hcloud_server" "bench" {
  name         = "pibake-bench"
  image        = "ubuntu-24.04"
  server_type  = var.server_type
  location     = var.location
  ssh_keys     = [var.ssh_key_name]
  firewall_ids = [hcloud_firewall.bench.id]
  user_data    = file("${path.module}/cloud-init.yaml")

  public_net {
    ipv4_enabled = true
    ipv6_enabled = true
  }

  labels = local.labels
}

locals {
  labels = {
    app        = "pibake-bench"
    managed_by = "terraform"
  }
}

output "ipv4" {
  value = hcloud_server.bench.ipv4_address
}
