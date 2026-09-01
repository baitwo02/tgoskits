// SPDX-License-Identifier: GPL-2.0-only
/*
 * UIO driver for the AxVisor ivshmem PCI profile.
 *
 * The driver requires exactly one MSI-X vector. It never falls back to MSI
 * or INTx, and it leaves protocol state in BAR0/BAR2 for userspace.
 */

#include <linux/interrupt.h>
#include <linux/io.h>
#include <linux/module.h>
#include <linux/pci.h>
#include <linux/spinlock.h>
#include <linux/uio_driver.h>

#define IVSHMEM_VENDOR_ID 0x1af4
#define IVSHMEM_DEVICE_ID 0x1110
#define IVSHMEM_REVISION 0x01

#define IVSHMEM_BAR_REGISTERS 0
#define IVSHMEM_BAR_SHARED 2
#define IVSHMEM_REG_INTERRUPT_CTRL 0x08
#define IVSHMEM_REG_EVENT_STATUS 0x14
#define IVSHMEM_INTERRUPT_ENABLE BIT(0)
#define IVSHMEM_EVENT_PENDING BIT(0)

struct ivshmem_uio {
	struct pci_dev *pdev;
	struct uio_info info;
	void __iomem *registers;
	spinlock_t irq_lock;
	bool irq_disabled;
};

static irqreturn_t ivshmem_uio_irq_handler(int irq, struct uio_info *info)
{
	struct ivshmem_uio *device = info->priv;
	unsigned long flags;

	if (!(readl(device->registers + IVSHMEM_REG_EVENT_STATUS) &
	      IVSHMEM_EVENT_PENDING))
		return IRQ_NONE;

	spin_lock_irqsave(&device->irq_lock, flags);
	if (!device->irq_disabled) {
		disable_irq_nosync(irq);
		device->irq_disabled = true;
	}
	spin_unlock_irqrestore(&device->irq_lock, flags);
	return IRQ_HANDLED;
}

static int ivshmem_uio_irqcontrol(struct uio_info *info, s32 irq_on)
{
	struct ivshmem_uio *device = info->priv;
	unsigned long flags;
	bool enable = false;
	bool disable = false;

	spin_lock_irqsave(&device->irq_lock, flags);
	if (irq_on && device->irq_disabled) {
		device->irq_disabled = false;
		enable = true;
	} else if (!irq_on && !device->irq_disabled) {
		device->irq_disabled = true;
		disable = true;
	}
	spin_unlock_irqrestore(&device->irq_lock, flags);

	if (enable)
		enable_irq(info->irq);
	else if (disable)
		disable_irq_nosync(info->irq);
	return 0;
}

static void ivshmem_uio_fill_memory(struct uio_mem *memory,
				    const char *name,
				    struct pci_dev *pdev, int bar)
{
	memory->name = name;
	memory->addr = pci_resource_start(pdev, bar);
	memory->size = pci_resource_len(pdev, bar);
	memory->memtype = UIO_MEM_PHYS;
}

static int ivshmem_uio_probe(struct pci_dev *pdev,
			     const struct pci_device_id *id)
{
	struct ivshmem_uio *device;
	int result;

	if (pdev->revision != IVSHMEM_REVISION)
		return -ENODEV;

	device = devm_kzalloc(&pdev->dev, sizeof(*device), GFP_KERNEL);
	if (!device)
		return -ENOMEM;
	device->pdev = pdev;
	spin_lock_init(&device->irq_lock);

	result = pci_enable_device_mem(pdev);
	if (result)
		return result;
	result = pci_request_regions(pdev, "uio_ivshmem");
	if (result)
		goto disable_device;
	result = pci_alloc_irq_vectors(pdev, 1, 1, PCI_IRQ_MSIX);
	if (result < 0)
		goto release_regions;
	if (result != 1) {
		result = -ENOSPC;
		goto free_vectors;
	}

	device->registers = pci_iomap(pdev, IVSHMEM_BAR_REGISTERS, 0);
	if (!device->registers) {
		result = -ENOMEM;
		goto free_vectors;
	}

	device->info.name = "uio_ivshmem";
	device->info.version = "1";
	device->info.irq = pci_irq_vector(pdev, 0);
	device->info.handler = ivshmem_uio_irq_handler;
	device->info.irqcontrol = ivshmem_uio_irqcontrol;
	device->info.priv = device;
	ivshmem_uio_fill_memory(&device->info.mem[0], "registers", pdev,
				 IVSHMEM_BAR_REGISTERS);
	ivshmem_uio_fill_memory(&device->info.mem[1], "shared", pdev,
				 IVSHMEM_BAR_SHARED);

	result = uio_register_device(&pdev->dev, &device->info);
	if (result)
		goto unmap_registers;
	pci_set_drvdata(pdev, device);
	writel(IVSHMEM_INTERRUPT_ENABLE,
	       device->registers + IVSHMEM_REG_INTERRUPT_CTRL);
	return 0;

unmap_registers:
	pci_iounmap(pdev, device->registers);
free_vectors:
	pci_free_irq_vectors(pdev);
release_regions:
	pci_release_regions(pdev);
disable_device:
	pci_disable_device(pdev);
	return result;
}

static void ivshmem_uio_remove(struct pci_dev *pdev)
{
	struct ivshmem_uio *device = pci_get_drvdata(pdev);
	unsigned long flags;
	bool enable = false;

	writel(0, device->registers + IVSHMEM_REG_INTERRUPT_CTRL);
	uio_unregister_device(&device->info);

	spin_lock_irqsave(&device->irq_lock, flags);
	if (device->irq_disabled) {
		device->irq_disabled = false;
		enable = true;
	}
	spin_unlock_irqrestore(&device->irq_lock, flags);
	if (enable)
		enable_irq(device->info.irq);

	pci_iounmap(pdev, device->registers);
	pci_free_irq_vectors(pdev);
	pci_release_regions(pdev);
	pci_disable_device(pdev);
}

static const struct pci_device_id ivshmem_uio_ids[] = {
	{ PCI_DEVICE(IVSHMEM_VENDOR_ID, IVSHMEM_DEVICE_ID) },
	{ }
};
MODULE_DEVICE_TABLE(pci, ivshmem_uio_ids);

static struct pci_driver ivshmem_uio_driver = {
	.name = "uio_ivshmem",
	.id_table = ivshmem_uio_ids,
	.probe = ivshmem_uio_probe,
	.remove = ivshmem_uio_remove,
};
module_pci_driver(ivshmem_uio_driver);

MODULE_AUTHOR("The AxVisor Team");
MODULE_DESCRIPTION("UIO driver for the AxVisor ivshmem PCI profile");
MODULE_LICENSE("GPL");
