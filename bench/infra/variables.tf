variable "hcloud_token" {
  description = "Hetzner Cloud API token"
  type        = string
  sensitive   = true
}

variable "ssh_key_name" {
  description = "Name of an SSH key already in the Hetzner Cloud project"
  type        = string
}

variable "server_type" {
  description = "Dedicated-vCPU type, so neighbours do not steal CPU time"
  type        = string
  default     = "ccx13"
}

variable "location" {
  type    = string
  default = "fsn1"
}

variable "ssh_source_ips" {
  type    = list(string)
  default = ["0.0.0.0/0", "::/0"]
}
