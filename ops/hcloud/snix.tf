# Hetzner cloud configuration for snix

terraform {
  required_providers {
    hcloud = {
      source = "hetznercloud/hcloud"
    }
  }

  backend "s3" {
    endpoints = {
      s3 = "https://s3.dualstack.eu-central-1.amazonaws.com"
    }

    bucket = "snix-tfstate"
    key    = "terraform/snix-hcloud"
    region = "eu-central-1"

    skip_credentials_validation = true
    skip_metadata_api_check     = true
    skip_requesting_account_id  = true
  }
}

provider "hcloud" {}

resource "hcloud_ssh_key" "raito" {
  name       = "raito"
  public_key = file("./raito.pub")
}

# TODO: pipe it from nix ssh keys
#
resource "hcloud_server" "meta01" {
  name  = "meta01.infra.snix.dev"
  image = "debian-12"
  # Observability stacks can eat quite the amount of RAM.
  server_type = "cx32"
  location    = "fsn1"
  ssh_keys    = [hcloud_ssh_key.raito.id]
  public_net {
    ipv4_enabled = true
    ipv6_enabled = true
  }

  lifecycle {
    ignore_changes = [ssh_keys]
  }
}

resource "hcloud_rdns" "meta01-v6" {
  server_id  = hcloud_server.meta01.id
  ip_address = hcloud_server.meta01.ipv6_address
  dns_ptr    = "meta01.infra.snix.dev"
}

resource "hcloud_rdns" "meta01-v4" {
  server_id  = hcloud_server.meta01.id
  ip_address = hcloud_server.meta01.ipv4_address
  dns_ptr    = "meta01.infra.snix.dev"
}

resource "hcloud_floating_ip" "mail" {
  type        = "ipv4"
  server_id   = hcloud_server.public01.id
  description = "IPv4 for mail hosting"
}

resource "hcloud_server" "public01" {
  name        = "public01.infra.snix.dev"
  image       = "debian-12"
  server_type = "cx22"
  location    = "fsn1"
  ssh_keys    = [hcloud_ssh_key.raito.id]
  public_net {
    ipv4_enabled = true
    ipv6_enabled = true
  }
  lifecycle {
    ignore_changes = [ssh_keys]
  }
}

resource "hcloud_rdns" "public01-v4" {
  server_id  = hcloud_server.public01.id
  ip_address = hcloud_server.public01.ipv4_address
  dns_ptr    = "public01.infra.snix.dev"
}

resource "hcloud_rdns" "public01-v6" {
  server_id  = hcloud_server.public01.id
  ip_address = hcloud_server.public01.ipv6_address
  dns_ptr    = "public01.infra.snix.dev"
}

resource "hcloud_server" "gerrit01" {
  name        = "gerrit01.infra.snix.dev"
  image       = "debian-12"
  server_type = "cpx31"
  location    = "fsn1"
  ssh_keys    = [hcloud_ssh_key.raito.id]
  public_net {
    ipv4_enabled = true
    ipv6_enabled = true
  }
  lifecycle {
    ignore_changes = [ssh_keys]
  }
}

resource "hcloud_rdns" "gerrit01-v6" {
  server_id  = hcloud_server.gerrit01.id
  ip_address = hcloud_server.gerrit01.ipv6_address
  dns_ptr    = "gerrit01.infra.snix.dev"
}

resource "hcloud_rdns" "gerrit01-v4" {
  server_id  = hcloud_server.gerrit01.id
  ip_address = hcloud_server.gerrit01.ipv4_address
  dns_ptr    = "gerrit01.infra.snix.dev"
}
