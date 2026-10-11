module "helper" {
  source = "./modules/helper"
}

# The values the ledger sums.
variable "values" {
  type    = list(number)
  default = [1, 2]
}

locals {
  scaled = [for value in var.values : value * module.helper.factor]
}

output "total" {
  value = sum(local.scaled)
}
