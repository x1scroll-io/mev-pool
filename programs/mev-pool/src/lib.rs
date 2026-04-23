use anchor_lang::prelude::*;
use anchor_lang::system_program;

declare_id!("3sjTba51AmhtEpVYcDU4sh4rpfPm6YiTmVTJaJAjTJjW"); // replace after deploy

// ── CONSTANTS (immutable once deployed) ──────────────────────────────────────
const TREASURY: &str = "A1TRS3i2g62Zf6K4vybsW4JLx8wifqSoThyTQqXNaLDK";
const BURN_ADDRESS: &str = "1nc1nerator11111111111111111111111111111111";

// x1scroll takes 10% of every pool distribution as operator fee
const OPERATOR_FEE_BPS: u64 = 1000;  // 10%
const BASIS_POINTS: u64 = 10000;

// Fee split on operator fee: 50% treasury / 50% burned
const TREASURY_BPS: u64 = 5000;
const BURN_BPS: u64 = 5000;

// Registration fee: 5 XNT to join pool
const REGISTRATION_FEE: u64 = 5_000_000_000;

// Max validators in pool
const MAX_POOL_MEMBERS: usize = 200;

// Minimum MEV contribution to trigger distribution (1 XNT)
const MIN_DISTRIBUTION: u64 = 1_000_000_000;

#[program]
pub mod mev_pool {
    use super::*;

    /// Initialize the MEV smoothing pool (called once by x1scroll)
    pub fn initialize(ctx: Context<Initialize>) -> Result<()> {
        let pool = &mut ctx.accounts.pool;
        pool.authority = ctx.accounts.authority.key();
        pool.member_count = 0;
        pool.total_contributed = 0;
        pool.total_distributed = 0;
        pool.total_operator_fees = 0;
        pool.total_burned = 0;
        pool.current_epoch_balance = 0;
        pool.last_distribution_epoch = 0;
        pool.bump = ctx.bumps.pool;
        Ok(())
    }

    /// Validator joins the MEV smoothing pool
    /// Pays 5 XNT registration fee
    pub fn join_pool(ctx: Context<JoinPool>) -> Result<()> {
        let pool = &mut ctx.accounts.pool;
        require!(
            (pool.member_count as usize) < MAX_POOL_MEMBERS,
            PoolError::PoolFull
        );

        // Check not already a member
        let identity = ctx.accounts.validator_identity.key();
        for i in 0..pool.member_count as usize {
            require!(
                pool.members[i].identity != identity,
                PoolError::AlreadyMember
            );
        }

        // Pay registration fee to treasury
        system_program::transfer(
            CpiContext::new(
                ctx.accounts.system_program.to_account_info(),
                system_program::Transfer {
                    from: ctx.accounts.validator_identity.to_account_info(),
                    to: ctx.accounts.treasury.to_account_info(),
                },
            ),
            REGISTRATION_FEE,
        )?;

        // Add member
        let idx = pool.member_count as usize;
        pool.members[idx] = PoolMember {
            identity,
            payout_wallet: ctx.accounts.payout_wallet.key(),
            total_contributed: 0,
            total_received: 0,
            joined_epoch: Clock::get()?.epoch,
            active: true,
        };
        pool.member_count += 1;

        emit!(ValidatorJoined {
            identity,
            epoch: Clock::get()?.epoch,
        });

        Ok(())
    }

    /// Validator contributes MEV earnings to the pool
    /// Called after each epoch where MEV was captured
    pub fn contribute_mev(ctx: Context<ContributeMev>, amount: u64) -> Result<()> {
        require!(amount > 0, PoolError::ZeroContribution);

        // Verify contributor is a pool member
        let pool = &mut ctx.accounts.pool;
        let identity = ctx.accounts.contributor.key();
        let mut member_idx = None;
        for i in 0..pool.member_count as usize {
            if pool.members[i].identity == identity && pool.members[i].active {
                member_idx = Some(i);
                break;
            }
        }
        require!(member_idx.is_some(), PoolError::NotAMember);

        // Transfer contribution to pool vault
        system_program::transfer(
            CpiContext::new(
                ctx.accounts.system_program.to_account_info(),
                system_program::Transfer {
                    from: ctx.accounts.contributor.to_account_info(),
                    to: ctx.accounts.pool_vault.to_account_info(),
                },
            ),
            amount,
        )?;

        pool.members[member_idx.unwrap()].total_contributed += amount;
        pool.total_contributed += amount;
        pool.current_epoch_balance += amount;

        emit!(MevContributed {
            contributor: identity,
            amount,
            epoch: Clock::get()?.epoch,
            pool_balance: pool.current_epoch_balance,
        });

        Ok(())
    }

    /// Distribute pool balance equally to all active members
    /// Can be called by anyone once per epoch
    pub fn distribute(ctx: Context<Distribute>) -> Result<()> {
        let pool = &mut ctx.accounts.pool;
        let current_epoch = Clock::get()?.epoch;

        // One distribution per epoch
        require!(
            current_epoch > pool.last_distribution_epoch,
            PoolError::AlreadyDistributedThisEpoch
        );
        require!(
            pool.current_epoch_balance >= MIN_DISTRIBUTION,
            PoolError::InsufficientBalance
        );

        let active_members: Vec<usize> = (0..pool.member_count as usize)
            .filter(|&i| pool.members[i].active)
            .collect();
        require!(!active_members.is_empty(), PoolError::NoActiveMembers);

        let total = pool.current_epoch_balance;

        // x1scroll operator fee: 10%
        let operator_fee = total * OPERATOR_FEE_BPS / BASIS_POINTS;
        let treasury_fee = operator_fee * TREASURY_BPS / BASIS_POINTS;
        let burn_fee = operator_fee - treasury_fee;
        let distributable = total - operator_fee;

        // Equal share per active member
        let share_per_member = distributable / active_members.len() as u64;
        let remainder = distributable - (share_per_member * active_members.len() as u64);

        // Pay operator fee to treasury
        system_program::transfer(
            CpiContext::new(
                ctx.accounts.system_program.to_account_info(),
                system_program::Transfer {
                    from: ctx.accounts.pool_vault.to_account_info(),
                    to: ctx.accounts.treasury.to_account_info(),
                },
            ),
            treasury_fee,
        )?;

        // Burn
        system_program::transfer(
            CpiContext::new(
                ctx.accounts.system_program.to_account_info(),
                system_program::Transfer {
                    from: ctx.accounts.pool_vault.to_account_info(),
                    to: ctx.accounts.burn_address.to_account_info(),
                },
            ),
            burn_fee,
        )?;

        // Update stats
        pool.total_distributed += distributable;
        pool.total_operator_fees += operator_fee;
        pool.total_burned += burn_fee;
        pool.last_distribution_epoch = current_epoch;
        pool.current_epoch_balance = remainder; // carry remainder to next epoch

        // Update member totals
        for &idx in &active_members {
            pool.members[idx].total_received += share_per_member;
        }

        emit!(PoolDistributed {
            epoch: current_epoch,
            total_pool: total,
            operator_fee,
            per_member: share_per_member,
            member_count: active_members.len() as u32,
            burned: burn_fee,
        });

        Ok(())
    }

    /// Validator leaves the pool
    pub fn leave_pool(ctx: Context<LeavePool>) -> Result<()> {
        let pool = &mut ctx.accounts.pool;
        let identity = ctx.accounts.validator_identity.key();

        for i in 0..pool.member_count as usize {
            if pool.members[i].identity == identity {
                pool.members[i].active = false;
                emit!(ValidatorLeft { identity, epoch: Clock::get()?.epoch });
                return Ok(());
            }
        }
        Err(PoolError::NotAMember.into())
    }
}

// ── ACCOUNTS ──────────────────────────────────────────────────────────────────

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(init, payer = authority, space = 8 + MevPool::LEN, seeds = [b"mev-pool"], bump)]
    pub pool: Account<'info, MevPool>,
    #[account(mut)]
    pub authority: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct JoinPool<'info> {
    #[account(mut, seeds = [b"mev-pool"], bump = pool.bump)]
    pub pool: Account<'info, MevPool>,
    #[account(mut)]
    pub validator_identity: Signer<'info>,
    /// CHECK: payout wallet for distributions
    pub payout_wallet: AccountInfo<'info>,
    /// CHECK: treasury
    #[account(mut, constraint = treasury.key().to_string() == TREASURY @ PoolError::InvalidTreasury)]
    pub treasury: AccountInfo<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ContributeMev<'info> {
    #[account(mut, seeds = [b"mev-pool"], bump = pool.bump)]
    pub pool: Account<'info, MevPool>,
    #[account(mut)]
    pub contributor: Signer<'info>,
    /// CHECK: pool vault holds contributions
    #[account(mut, seeds = [b"mev-vault"], bump)]
    pub pool_vault: AccountInfo<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Distribute<'info> {
    #[account(mut, seeds = [b"mev-pool"], bump = pool.bump)]
    pub pool: Account<'info, MevPool>,
    /// CHECK: pool vault
    #[account(mut, seeds = [b"mev-vault"], bump)]
    pub pool_vault: AccountInfo<'info>,
    /// CHECK: treasury
    #[account(mut, constraint = treasury.key().to_string() == TREASURY @ PoolError::InvalidTreasury)]
    pub treasury: AccountInfo<'info>,
    /// CHECK: burn
    #[account(mut, constraint = burn_address.key().to_string() == BURN_ADDRESS @ PoolError::InvalidBurnAddress)]
    pub burn_address: AccountInfo<'info>,
    pub caller: Signer<'info>, // anyone can trigger distribution
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct LeavePool<'info> {
    #[account(mut, seeds = [b"mev-pool"], bump = pool.bump)]
    pub pool: Account<'info, MevPool>,
    pub validator_identity: Signer<'info>,
}

// ── STATE ─────────────────────────────────────────────────────────────────────

#[account]
pub struct MevPool {
    pub authority: Pubkey,
    pub member_count: u32,
    pub total_contributed: u64,
    pub total_distributed: u64,
    pub total_operator_fees: u64,
    pub total_burned: u64,
    pub current_epoch_balance: u64,
    pub last_distribution_epoch: u64,
    pub bump: u8,
    pub members: [PoolMember; 200],
}

impl MevPool {
    pub const LEN: usize = 32 + 4 + 8 + 8 + 8 + 8 + 8 + 8 + 1 + (PoolMember::LEN * 200);
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy)]
pub struct PoolMember {
    pub identity: Pubkey,
    pub payout_wallet: Pubkey,
    pub total_contributed: u64,
    pub total_received: u64,
    pub joined_epoch: u64,
    pub active: bool,
}

impl PoolMember {
    pub const LEN: usize = 32 + 32 + 8 + 8 + 8 + 1;
}

// ── EVENTS ────────────────────────────────────────────────────────────────────

#[event]
pub struct ValidatorJoined { pub identity: Pubkey, pub epoch: u64 }

#[event]
pub struct ValidatorLeft { pub identity: Pubkey, pub epoch: u64 }

#[event]
pub struct MevContributed {
    pub contributor: Pubkey,
    pub amount: u64,
    pub epoch: u64,
    pub pool_balance: u64,
}

#[event]
pub struct PoolDistributed {
    pub epoch: u64,
    pub total_pool: u64,
    pub operator_fee: u64,
    pub per_member: u64,
    pub member_count: u32,
    pub burned: u64,
}

// ── ERRORS ────────────────────────────────────────────────────────────────────

#[error_code]
pub enum PoolError {
    #[msg("Pool is full — max 200 validators")]
    PoolFull,
    #[msg("Already a pool member")]
    AlreadyMember,
    #[msg("Not a pool member")]
    NotAMember,
    #[msg("Zero contribution not allowed")]
    ZeroContribution,
    #[msg("Already distributed this epoch")]
    AlreadyDistributedThisEpoch,
    #[msg("Pool balance below minimum distribution threshold")]
    InsufficientBalance,
    #[msg("No active members in pool")]
    NoActiveMembers,
    #[msg("Invalid treasury address")]
    InvalidTreasury,
    #[msg("Invalid burn address")]
    InvalidBurnAddress,
}
